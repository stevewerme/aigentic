//! `aigentic init`: the guided setup, in the spirit of upstream's
//! `setup-matt-pocock-skills`: explore, show, confirm, write. Four
//! sections (config, project, GitHub, knowledge), each showing what exists
//! and proposing a default the user accepts with Enter. Every file is
//! shown before it is written; nothing reaches GitHub without a yes; a
//! rerun shows current values as defaults and writes only what changed.
//! `gh` goes through the `Gh` trait from `checks.rs`, answers through
//! `Ask`, so the pure parts are tested with scripts and no terminal.

use std::collections::BTreeMap;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use aigentic_runtime::project::{
    DOT_DIR, FILE_NAME, INSTRUCTIONS_FILE, KNOWLEDGE_DIR, MEMORY_DIR, PocockSection,
};
use aigentic_runtime::{Project, ProjectFile};
use anyhow::{Context, bail};

use crate::checks::{
    Gh, GhCli, Status, check_api_key_env, check_config, is_github_remote, origin_url,
};
use crate::config::{Config, EXAMPLE};
use crate::pocock::{self, Outcome, TRIAGE_ROLES};
use crate::project_cmd::init_template;

/// Where answers come from: the terminal, or a script in tests.
pub trait Ask {
    /// Text for the user: what exists, what is proposed.
    fn show(&mut self, text: &str);
    /// A free-text question; Enter accepts `default`.
    fn ask(&mut self, prompt: &str, default: &str) -> anyhow::Result<String>;
    /// A yes/no question; Enter accepts `default`.
    fn confirm(&mut self, prompt: &str, default: bool) -> anyhow::Result<bool>;
}

/// The terminal.
pub struct Terminal;

impl Terminal {
    fn read_line(&self) -> anyhow::Result<String> {
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        Ok(line.trim().to_owned())
    }
}

impl Ask for Terminal {
    fn show(&mut self, text: &str) {
        println!("{text}");
    }
    fn ask(&mut self, prompt: &str, default: &str) -> anyhow::Result<String> {
        if default.is_empty() {
            print!("{prompt}: ");
        } else {
            print!("{prompt} [{default}]: ");
        }
        let line = self.read_line()?;
        Ok(if line.is_empty() {
            default.to_owned()
        } else {
            line
        })
    }
    fn confirm(&mut self, prompt: &str, default: bool) -> anyhow::Result<bool> {
        print!("{prompt} [{}] ", if default { "Y/n" } else { "y/N" });
        let line = self.read_line()?.to_ascii_lowercase();
        Ok(match line.as_str() {
            "" => default,
            "y" | "yes" => true,
            _ => false,
        })
    }
}

/// What a run did, for the summary and the tests.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    /// Files written, relative to the project root (the config by its
    /// full path).
    pub written: Vec<PathBuf>,
    pub labels_created: Vec<String>,
    /// Symlinks made under `.aigentic/knowledge/`.
    pub linked: Vec<PathBuf>,
}

/// `aigentic init` at the terminal.
pub fn run(config_path: &Path, cwd: &Path) -> anyhow::Result<i32> {
    if !std::io::stdin().is_terminal() {
        bail!("aigentic init is interactive and needs a terminal on stdin");
    }
    let root = aigentic_runtime::project::find_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    let remote = origin_url(&root);
    let report = run_with(config_path, cwd, &mut Terminal, &GhCli, remote.as_deref())?;
    println!();
    if report.written.is_empty() && report.labels_created.is_empty() && report.linked.is_empty() {
        println!("nothing to change");
    } else {
        for p in &report.written {
            println!("wrote    {}", p.display());
        }
        for l in &report.labels_created {
            println!("label    {l}");
        }
        for p in &report.linked {
            println!("linked   {}", p.display());
        }
    }
    println!("run `aigentic doctor` to check the result");
    Ok(0)
}

/// The setup over any answer source and `gh`. `remote` is the origin
/// URL, `None` when there is none.
pub fn run_with(
    config_path: &Path,
    cwd: &Path,
    ask: &mut dyn Ask,
    gh: &dyn Gh,
    remote: Option<&str>,
) -> anyhow::Result<Report> {
    let mut report = Report::default();

    // 1. Config.
    ask.show("== config ==");
    let config = config_section(config_path, ask, &mut report)?;

    // 2. Project.
    ask.show("\n== project ==");
    let root = aigentic_runtime::project::find_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    let project_file = root.join(FILE_NAME);
    let existing_text = read_if_present(&project_file)?;
    let existing = existing_text
        .as_deref()
        .map(ProjectFile::parse)
        .transpose()
        .map_err(|e| anyhow::anyhow!("{}: {e}", project_file.display()))?;
    match &existing_text {
        Some(_) if root != cwd => ask.show(&format!(
            "project file {} (above the working directory)",
            project_file.display()
        )),
        Some(_) => ask.show(&format!("project file {}", project_file.display())),
        None => ask.show(&format!("no {FILE_NAME} here or above; creating one")),
    }
    let dir_name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".into());
    let current_name = existing
        .as_ref()
        .and_then(|f| f.project.as_ref())
        .map(|p| p.name.clone())
        .filter(|n| !n.trim().is_empty())
        .unwrap_or(dir_name);
    let name = ask.ask("project name", &current_name)?;
    let current_desc = existing
        .as_ref()
        .and_then(|f| f.project.as_ref())
        .and_then(|p| p.description.clone())
        .unwrap_or_default();
    let description = ask.ask("description", &current_desc)?;
    let current_profile = existing
        .as_ref()
        .and_then(|f| f.model.as_ref())
        .map_or(config.default_profile.clone(), |m| m.profile.clone());
    let profile = ask.ask("[model] profile", &current_profile)?;
    if !config.profiles.contains_key(&profile) {
        ask.show(&format!(
            "note: {profile:?} is not a profile in the config (defined: {})",
            config
                .profiles
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let mut text = render_project_file(existing_text.as_deref(), &name, &description, &profile);
    propose(&root, &project_file, &text, ask, &mut report)?;
    for dir in [KNOWLEDGE_DIR, MEMORY_DIR] {
        let path = root.join(DOT_DIR).join(dir);
        if !path.is_dir() {
            std::fs::create_dir_all(&path)
                .with_context(|| format!("creating {}", path.display()))?;
            std::fs::write(path.join(".gitkeep"), "")?;
            ask.show(&format!("created {}/{dir}", DOT_DIR));
        }
    }
    let instructions = root.join(DOT_DIR).join(INSTRUCTIONS_FILE);
    let agents = root.join("AGENTS.md");
    if !instructions.is_file() && !agents.is_file() {
        ask.show(&format!(
            "no {DOT_DIR}/{INSTRUCTIONS_FILE} or AGENTS.md: proposing AGENTS.md"
        ));
        propose(
            &root,
            &agents,
            &render_agents_md(&name, &description),
            ask,
            &mut report,
        )?;
    } else {
        ask.show(&format!(
            "instructions: {}",
            if instructions.is_file() {
                format!("{DOT_DIR}/{INSTRUCTIONS_FILE}")
            } else {
                "AGENTS.md".to_owned()
            }
        ));
    }

    // 3. GitHub.
    ask.show("\n== github ==");
    let current: Option<ProjectFile> = read_if_present(&project_file)?
        .as_deref()
        .map(ProjectFile::parse)
        .transpose()
        .map_err(|e| anyhow::anyhow!("{}: {e}", project_file.display()))?;
    let tracker = current
        .as_ref()
        .and_then(|f| f.pocock.as_ref())
        .map(|p| p.issue_tracker.clone());
    let github_remote = remote.is_some_and(is_github_remote);
    let authenticated = github_remote && gh.run(&["auth", "status"]).is_ok();
    match (&tracker, github_remote, authenticated) {
        (Some(t), _, _) if t != "github" => {
            ask.show(&format!("[pocock] issue_tracker = {t:?} kept"));
        }
        (_, false, _) => ask.show(
            match remote {
                Some(url) => format!("skip: origin {url} is not on GitHub"),
                None => "skip: no git remote named origin".to_owned(),
            }
            .as_str(),
        ),
        (_, true, false) => ask.show("skip: `gh auth status` failed; run `gh auth login`"),
        (_, true, true) => {
            let wanted = tracker.is_some()
                || ask.confirm(
                    "use GitHub Issues as the issue tracker ([pocock] issue_tracker = \"github\")?",
                    true,
                )?;
            if wanted {
                if tracker.is_none() {
                    text = set_toml_key(&text, "pocock", "issue_tracker", "\"github\"");
                    propose(&root, &project_file, &text, ask, &mut report)?;
                }
                let project = Project::open_root(&root)?;
                if let Some(pocock) = project.file.pocock.as_ref() {
                    github_section(&project, pocock, ask, gh, &mut report)?;
                }
            }
        }
    }

    // 4. Knowledge.
    ask.show("\n== knowledge ==");
    let links = knowledge_links(&root);
    if links.is_empty() {
        ask.show("nothing to link: no docs/ or CONTEXT.md, or already linked");
    }
    for (link, target) in links {
        let rel = link.strip_prefix(&root).unwrap_or(&link).to_path_buf();
        if ask.confirm(
            &format!("link {} -> {}?", rel.display(), target.display()),
            true,
        )? {
            std::os::unix::fs::symlink(&target, &link)
                .with_context(|| format!("linking {}", link.display()))?;
            report.linked.push(rel);
        }
    }
    Ok(report)
}

fn config_section(
    config_path: &Path,
    ask: &mut dyn Ask,
    report: &mut Report,
) -> anyhow::Result<Config> {
    if !config_path.is_file() {
        ask.show(&format!(
            "no config at {}; proposing the example:\n\n{EXAMPLE}",
            config_path.display()
        ));
        if !ask.confirm("write it?", true)? {
            bail!("a config is needed to continue");
        }
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(config_path, EXAMPLE)
            .with_context(|| format!("writing {}", config_path.display()))?;
        report.written.push(config_path.to_path_buf());
    }
    let (check, config) = check_config(config_path);
    ask.show(&check.render());
    let Some(mut config) = config else {
        bail!("fix the config and rerun");
    };
    if config.profiles.len() > 1 {
        let names: Vec<String> = config.profiles.keys().cloned().collect();
        let chosen = ask.ask(
            &format!("default profile ({})", names.join(", ")),
            &config.default_profile,
        )?;
        if !config.profiles.contains_key(&chosen) {
            bail!("no profile {chosen:?}; defined: {}", names.join(", "));
        }
        if chosen != config.default_profile {
            let text = std::fs::read_to_string(config_path)?;
            let text = set_toml_key(&text, "", "default_profile", &format!("{chosen:?}"));
            ask.show(&format!("--- {}\n{text}", config_path.display()));
            if ask.confirm("write it?", true)? {
                std::fs::write(config_path, &text)?;
                report.written.push(config_path.to_path_buf());
                config.default_profile = chosen;
            }
        }
    }
    for (name, profile) in &config.profiles {
        let check = check_api_key_env(name, profile);
        ask.show(&check.render());
        if check.status == Status::Fail {
            ask.show(&format!(
                "  set {} in .env or the environment before starting a thread",
                profile.api_key_env
            ));
        }
    }
    Ok(config)
}

fn github_section(
    project: &Project,
    pocock: &PocockSection,
    ask: &mut dyn Ask,
    gh: &dyn Gh,
    report: &mut Report,
) -> anyhow::Result<()> {
    let triage = project.file.skills.enabled.iter().any(|s| s == "triage");
    let files = pocock::render(pocock, triage)?;
    for f in &files {
        propose(
            &project.root,
            &project.root.join(&f.path),
            &f.text,
            ask,
            report,
        )?;
    }
    let block = pocock::agent_skills_block(pocock, &files);
    let ours = project.root.join(DOT_DIR).join(INSTRUCTIONS_FILE);
    let instructions = if ours.is_file() {
        ours
    } else {
        project.root.join("AGENTS.md")
    };
    let existing = read_if_present(&instructions)?.unwrap_or_default();
    propose(
        &project.root,
        &instructions,
        &pocock::apply_block(&existing, &block),
        ask,
        report,
    )?;

    let listed = match gh.run(&[
        "label", "list", "--limit", "200", "--json", "name", "--jq", ".[].name",
    ]) {
        Ok(out) => parse_label_list(&out),
        Err(e) => {
            ask.show(&format!("skip labels: {e}"));
            return Ok(());
        }
    };
    let missing = missing_labels(&listed, &pocock.triage_labels);
    if missing.is_empty() {
        ask.show("triage labels: all five exist");
        return Ok(());
    }
    ask.show(&format!(
        "triage labels missing on GitHub:\n{}",
        missing
            .iter()
            .map(|(l, m)| format!("  {l:<18} {m}"))
            .collect::<Vec<_>>()
            .join("\n")
    ));
    if !ask.confirm(
        &format!("create {} label(s) with `gh label create`?", missing.len()),
        true,
    )? {
        return Ok(());
    }
    for (label, meaning) in missing {
        match gh.run(&["label", "create", &label, "--description", meaning]) {
            Ok(_) => report.labels_created.push(label),
            Err(e) => ask.show(&format!("  {label}: {e}")),
        }
    }
    Ok(())
}

/// Show `text` for `path` and write it on a yes, unless the file already
/// holds it. Records the relative path in `report`.
fn propose(
    root: &Path,
    path: &Path,
    text: &str,
    ask: &mut dyn Ask,
    report: &mut Report,
) -> anyhow::Result<()> {
    let rel = path.strip_prefix(root).unwrap_or(path).to_path_buf();
    let current = read_if_present(path)?;
    if current.as_deref() == Some(text) {
        ask.show(&format!("{} unchanged", rel.display()));
        return Ok(());
    }
    let verb = if current.is_some() {
        "update"
    } else {
        "create"
    };
    ask.show(&format!(
        "--- {} ({verb})\n{}",
        rel.display(),
        text.trim_end()
    ));
    if ask.confirm(&format!("{verb} {}?", rel.display()), true)? {
        if pocock::write_if_changed(path, text)? == Outcome::Written {
            report.written.push(rel);
        }
    } else {
        ask.show(&format!("skipped {}", rel.display()));
    }
    Ok(())
}

fn read_if_present(path: &Path) -> anyhow::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// The project file for the answers: the existing text with the three
/// keys set, or the `project init` template for a new project. Hand
/// edits elsewhere in the file are kept.
pub fn render_project_file(
    existing: Option<&str>,
    name: &str,
    description: &str,
    profile: &str,
) -> String {
    let mut text = existing.map_or_else(|| init_template(name), str::to_owned);
    text = set_toml_key(&text, "project", "name", &format!("{name:?}"));
    if !description.trim().is_empty() {
        text = set_toml_key(&text, "project", "description", &format!("{description:?}"));
    }
    set_toml_key(&text, "model", "profile", &format!("{profile:?}"))
}

/// A first instructions file: a heading, the description and a
/// "Commands" section for the agent to run.
pub fn render_agents_md(name: &str, description: &str) -> String {
    let mut out = format!("# {name}\n\n");
    if !description.trim().is_empty() {
        out.push_str(&format!("{}\n\n", description.trim()));
    }
    out.push_str(
        "## Commands\n\n\
         The commands that must pass before a change is finished, for example\n\
         build, test and lint. One per code block.\n\n\
         ```bash\n\
         # replace with the test command\n\
         ```\n",
    );
    out
}

/// `key = value` under `[section]` in `text`: an existing line is
/// replaced, a commented-out `# key = ...` or `# [section]` from the
/// template is uncommented, a missing key is added under its header and
/// a missing section is appended. `section` empty means the top level,
/// before the first header. `value` is already TOML (quoted).
pub fn set_toml_key(text: &str, section: &str, key: &str, value: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let is_header = |l: &str| {
        let l = l.trim_start_matches('#').trim();
        l.starts_with('[') && l.ends_with(']')
    };
    let header_of = |l: &str| -> Option<String> {
        let l = l.trim_start_matches('#').trim();
        l.strip_prefix('[')
            .and_then(|l| l.strip_suffix(']'))
            .map(|s| s.trim().to_owned())
    };
    let key_of = |l: &str| -> Option<String> {
        let l = l.trim_start_matches('#').trim();
        l.split_once('=').map(|(k, _)| k.trim().to_owned())
    };
    let new_line = format!("{key} = {value}");
    let mut out: Vec<String> = Vec::new();
    // The span of the section: (start index after its header, end index).
    let mut start = None;
    let mut end = lines.len();
    if section.is_empty() {
        start = Some(0);
        end = lines
            .iter()
            .position(|l| is_header(l))
            .unwrap_or(lines.len());
    } else {
        for (i, l) in lines.iter().enumerate() {
            if let Some(h) = header_of(l) {
                if start.is_some() {
                    end = i;
                    break;
                }
                if h == section {
                    start = Some(i + 1);
                }
            }
        }
    }
    let Some(start) = start else {
        let mut text = text.trim_end().to_owned();
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&format!("[{section}]\n{new_line}\n"));
        return text;
    };
    let mut replaced = false;
    for (i, l) in lines.iter().enumerate() {
        if i >= start && i < end && !replaced && key_of(l).as_deref() == Some(key) {
            // Keep a trailing comment from the template.
            let comment = l
                .trim_start_matches('#')
                .split_once('=')
                .and_then(|(_, v)| v.find(" #").map(|p| v[p..].to_owned()))
                .unwrap_or_default();
            out.push(format!("{new_line}{comment}"));
            replaced = true;
        } else {
            out.push((*l).to_owned());
        }
    }
    if !replaced {
        let at = if section.is_empty() { 0 } else { start };
        out.insert(at, new_line);
    }
    // Uncomment the header when it came from the template.
    if !section.is_empty() {
        let h = start - 1;
        if out[h].trim_start().starts_with('#') {
            out[h] = format!("[{section}]");
        }
    }
    let mut result = out.join("\n");
    result.push('\n');
    result
}

/// One label name per line, blanks dropped.
pub fn parse_label_list(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The canonical triage labels, mapped through `overrides`, that `listed`
/// lacks: `(label, meaning)` in the table's order.
pub fn missing_labels(
    listed: &[String],
    overrides: &BTreeMap<String, String>,
) -> Vec<(String, &'static str)> {
    TRIAGE_ROLES
        .iter()
        .map(|(role, meaning)| {
            (
                overrides
                    .get(*role)
                    .map_or((*role).to_owned(), Clone::clone),
                *meaning,
            )
        })
        .filter(|(label, _)| !listed.iter().any(|l| l == label))
        .collect()
}

/// Symlinks to offer: `(link, target)` for `docs/` and `CONTEXT.md` when
/// they exist at `root` and nothing sits at the link's path yet.
pub fn knowledge_links(root: &Path) -> Vec<(PathBuf, PathBuf)> {
    let knowledge = root.join(DOT_DIR).join(KNOWLEDGE_DIR);
    ["docs", "CONTEXT.md"]
        .into_iter()
        .filter(|name| root.join(name).exists())
        .filter(|name| std::fs::symlink_metadata(knowledge.join(name)).is_err())
        .map(|name| (knowledge.join(name), Path::new("..").join("..").join(name)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Defaults everywhere unless `overrides` names a prompt fragment.
    struct Scripted {
        overrides: Vec<(&'static str, String)>,
        shown: Vec<String>,
    }
    impl Scripted {
        fn new(overrides: &[(&'static str, &str)]) -> Self {
            Self {
                overrides: overrides
                    .iter()
                    .map(|(k, v)| (*k, (*v).to_owned()))
                    .collect(),
                shown: Vec::new(),
            }
        }
        fn find(&self, prompt: &str) -> Option<&str> {
            self.overrides
                .iter()
                .find(|(k, _)| prompt.contains(k))
                .map(|(_, v)| v.as_str())
        }
    }
    impl Ask for Scripted {
        fn show(&mut self, text: &str) {
            self.shown.push(text.to_owned());
        }
        fn ask(&mut self, prompt: &str, default: &str) -> anyhow::Result<String> {
            Ok(self.find(prompt).unwrap_or(default).to_owned())
        }
        fn confirm(&mut self, prompt: &str, default: bool) -> anyhow::Result<bool> {
            Ok(self.find(prompt).map_or(default, |v| v == "y"))
        }
    }

    struct FakeGh {
        labels: RefCell<Vec<String>>,
        created: RefCell<Vec<Vec<String>>>,
        auth_ok: bool,
    }
    impl Gh for FakeGh {
        fn run(&self, args: &[&str]) -> anyhow::Result<String> {
            match args {
                ["auth", "status"] if self.auth_ok => Ok("ok".into()),
                ["auth", "status"] => anyhow::bail!("not logged in"),
                ["label", "list", ..] => Ok(self.labels.borrow().join("\n")),
                ["label", "create", name, "--description", _] => {
                    self.created
                        .borrow_mut()
                        .push(args.iter().map(|s| (*s).to_owned()).collect());
                    self.labels.borrow_mut().push((*name).to_owned());
                    Ok(String::new())
                }
                other => anyhow::bail!("unexpected gh {}", other.join(" ")),
            }
        }
    }

    #[test]
    fn label_diff_maps_roles_through_overrides() {
        let listed = vec![
            "bug".to_owned(),
            "needs-info".to_owned(),
            "wontfix".to_owned(),
        ];
        let missing = missing_labels(&listed, &BTreeMap::new());
        assert_eq!(
            missing.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>(),
            vec!["needs-triage", "ready-for-agent", "ready-for-human"]
        );
        assert_eq!(missing[0].1, "Maintainer needs to evaluate this issue");
        let overrides = BTreeMap::from([("needs-triage".to_owned(), "bug:triage".to_owned())]);
        let listed = vec!["bug:triage".to_owned()];
        let missing = missing_labels(&listed, &overrides);
        assert_eq!(missing.len(), 4);
        assert!(
            missing
                .iter()
                .all(|(l, _)| l != "needs-triage" && l != "bug:triage")
        );
        assert!(
            missing_labels(
                &parse_label_list(
                    " a\n\nneeds-triage\nneeds-info\nready-for-agent\nready-for-human\nwontfix\n"
                ),
                &BTreeMap::new()
            )
            .is_empty()
        );
    }

    #[test]
    fn toml_keys_are_set_uncommented_added_or_appended() {
        let t = init_template("x");
        let t = set_toml_key(&t, "project", "name", "\"y\"");
        assert!(t.contains("[project]\nname = \"y\"\n"), "{t}");
        let t = set_toml_key(&t, "project", "description", "\"d\"");
        assert!(t.contains("name = \"y\"\ndescription = \"d\"\n"), "{t}");
        assert!(!t.contains("# description"), "{t}");
        let t = set_toml_key(&t, "model", "profile", "\"anthropic\"");
        assert!(
            t.contains(
                "[model]\nprofile = \"anthropic\" # a profile in config.toml; --profile overrides\n"
            ),
            "{t}"
        );
        assert!(
            t.contains("# [tools]"),
            "other sections stay commented: {t}"
        );
        let t = set_toml_key(&t, "pocock", "issue_tracker", "\"github\"");
        assert!(
            t.ends_with("\n\n[pocock]\nissue_tracker = \"github\"\n"),
            "{t}"
        );
        // Idempotent.
        assert_eq!(set_toml_key(&t, "pocock", "issue_tracker", "\"github\""), t);
        // A key added to an existing section lands under its header.
        let t = set_toml_key("[a]\nx = 1\n\n[b]\ny = 2\n", "a", "z", "3");
        assert_eq!(t, "[a]\nz = 3\nx = 1\n\n[b]\ny = 2\n");
        // Top level.
        assert_eq!(
            set_toml_key(
                "default_profile = \"a\"\n\n[profiles.a]\nmodel = \"m\"\n",
                "",
                "default_profile",
                "\"b\""
            ),
            "default_profile = \"b\"\n\n[profiles.a]\nmodel = \"m\"\n"
        );
        assert_eq!(
            set_toml_key(
                "[profiles.a]\nmodel = \"m\"\n",
                "",
                "default_profile",
                "\"a\""
            ),
            "default_profile = \"a\"\n[profiles.a]\nmodel = \"m\"\n"
        );
    }

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("myapp");
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("docs/a.md"), "# a\n").unwrap();
        std::fs::write(root.join("CONTEXT.md"), "# ctx\n").unwrap();
        let config = dir.path().join("cfg/config.toml");
        (dir, root, config)
    }

    #[test]
    fn a_fresh_project_renders_every_file_and_a_rerun_changes_nothing() {
        let (_dir, root, config) = fixture();
        let gh = FakeGh {
            labels: RefCell::new(vec!["bug".into(), "wontfix".into()]),
            created: RefCell::new(Vec::new()),
            auth_ok: true,
        };
        let mut ask = Scripted::new(&[("description", "An app"), ("default profile", "anthropic")]);
        let report = run_with(
            &config,
            &root,
            &mut ask,
            &gh,
            Some("git@github.com:x/myapp.git"),
        )
        .unwrap();

        // Config from the example, default profile switched.
        let cfg = std::fs::read_to_string(&config).unwrap();
        assert!(
            cfg.starts_with("default_profile = \"anthropic\"\n"),
            "{cfg}"
        );
        assert!(report.written.contains(&config));
        // Project file with the answers and the tracker.
        let toml = std::fs::read_to_string(root.join(FILE_NAME)).unwrap();
        assert!(
            toml.contains("[project]\nname = \"myapp\"\ndescription = \"An app\"\n"),
            "{toml}"
        );
        assert!(toml.contains("[model]\nprofile = \"anthropic\""), "{toml}");
        assert!(
            toml.contains("[pocock]\nissue_tracker = \"github\"\n"),
            "{toml}"
        );
        assert!(root.join(".aigentic/knowledge/.gitkeep").is_file());
        assert!(root.join(".aigentic/memory/.gitkeep").is_file());
        // AGENTS.md: heading, Commands, then the agent skills block.
        let agents = std::fs::read_to_string(root.join("AGENTS.md")).unwrap();
        assert!(
            agents.starts_with("# myapp\n\nAn app\n\n## Commands\n"),
            "{agents}"
        );
        assert!(
            agents.contains("\n## Agent skills\n\n### Issue tracker\n"),
            "{agents}"
        );
        assert!(root.join("docs/agents/issue-tracker.md").is_file());
        assert!(root.join("docs/agents/domain.md").is_file());
        // Labels: the three missing ones, created in table order.
        assert_eq!(
            report.labels_created,
            vec![
                "needs-triage",
                "needs-info",
                "ready-for-agent",
                "ready-for-human"
            ]
        );
        let created = gh.created.borrow();
        assert_eq!(created.len(), 4);
        assert_eq!(created[0][2], "needs-triage");
        assert_eq!(created[0][4], "Maintainer needs to evaluate this issue");
        drop(created);
        // Knowledge links.
        assert_eq!(
            report.linked,
            vec![
                PathBuf::from(".aigentic/knowledge/docs"),
                PathBuf::from(".aigentic/knowledge/CONTEXT.md")
            ]
        );
        assert!(root.join(".aigentic/knowledge/docs/a.md").is_file());
        assert_eq!(
            std::fs::read_link(root.join(".aigentic/knowledge/CONTEXT.md")).unwrap(),
            PathBuf::from("../../CONTEXT.md")
        );
        let written: Vec<String> = report
            .written
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        assert!(written.iter().any(|p| p == "aigentic.toml"), "{written:?}");
        assert!(written.iter().any(|p| p == "AGENTS.md"), "{written:?}");
        assert!(
            written.iter().any(|p| p == "docs/agents/issue-tracker.md"),
            "{written:?}"
        );

        // Rerun from a subdirectory: defaults are the current values and
        // nothing is written, created or linked.
        let sub = root.join("src");
        std::fs::create_dir_all(&sub).unwrap();
        let before: Vec<(PathBuf, String)> =
            ["aigentic.toml", "AGENTS.md", "docs/agents/domain.md"]
                .iter()
                .map(|p| (root.join(p), std::fs::read_to_string(root.join(p)).unwrap()))
                .collect();
        let mut ask = Scripted::new(&[]);
        let report = run_with(
            &config,
            &sub,
            &mut ask,
            &gh,
            Some("https://github.com/x/myapp"),
        )
        .unwrap();
        assert_eq!(report, Report::default(), "{:#?}", ask.shown);
        assert!(gh.created.borrow().len() == 4, "no more labels");
        for (path, text) in before {
            assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        }
        assert!(
            ask.shown.iter().any(|s| s == "aigentic.toml unchanged"),
            "{:#?}",
            ask.shown
        );
        assert!(
            ask.shown
                .iter()
                .any(|s| s == "triage labels: all five exist")
        );

        // A changed answer rewrites only the project file.
        let mut ask = Scripted::new(&[("description", "Renamed")]);
        let report = run_with(
            &config,
            &root,
            &mut ask,
            &gh,
            Some("git@github.com:x/myapp.git"),
        )
        .unwrap();
        assert_eq!(report.written, vec![PathBuf::from("aigentic.toml")]);
        let toml = std::fs::read_to_string(root.join(FILE_NAME)).unwrap();
        assert!(toml.contains("description = \"Renamed\""), "{toml}");
        assert!(toml.contains("issue_tracker = \"github\""), "{toml}");
    }

    #[test]
    fn a_no_answer_and_a_non_github_remote_leave_things_alone() {
        let (_dir, root, config) = fixture();
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, "[profiles.only]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"AIGENTIC_INIT_TEST_KEY\"\n").unwrap();
        std::fs::write(root.join("AGENTS.md"), "# mine\n").unwrap();
        let gh = FakeGh {
            labels: RefCell::new(Vec::new()),
            created: RefCell::new(Vec::new()),
            auth_ok: true,
        };
        let mut ask = Scripted::new(&[("link .aigentic/knowledge/docs", "n")]);
        let report = run_with(
            &config,
            &root,
            &mut ask,
            &gh,
            Some("https://gitlab.com/x/y.git"),
        )
        .unwrap();
        assert_eq!(report.written, vec![PathBuf::from("aigentic.toml")]);
        assert_eq!(report.labels_created, Vec::<String>::new());
        assert_eq!(
            report.linked,
            vec![PathBuf::from(".aigentic/knowledge/CONTEXT.md")]
        );
        assert_eq!(
            std::fs::read_to_string(root.join("AGENTS.md")).unwrap(),
            "# mine\n"
        );
        let toml = std::fs::read_to_string(root.join(FILE_NAME)).unwrap();
        assert!(!toml.contains("[pocock]"), "{toml}");
        assert!(toml.contains("profile = \"only\""), "{toml}");
        assert!(
            ask.shown.iter().any(|s| s.contains("not on GitHub")),
            "{:#?}",
            ask.shown
        );
        assert!(
            ask.shown
                .iter()
                .any(|s| s.contains("AIGENTIC_INIT_TEST_KEY is not set"))
        );
        // Declining the config write stops the run without a file.
        let missing = root.join("nope/config.toml");
        let mut ask = Scripted::new(&[("write it?", "n")]);
        assert!(run_with(&missing, &root, &mut ask, &gh, None).is_err());
        assert!(!missing.exists());
    }
}
