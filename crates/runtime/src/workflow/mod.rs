//! The workflow folder: a `workflow.toml` and its `templates/`, resolved
//! project `.aigentic/workflows/<name>/`, then user, then bundled — the
//! order skills resolve in. A workflow is data a runner fills, step by
//! step, with the slots a step reports; it is never offered to a model
//! and never loaded as a skill (`docs/PLAN-layer2.md` §3).
//!
//! Loading is strict on purpose. Every fixed rule (role header, safety
//! line, gate, trailer, ledger format, `.env`) lives once in a template,
//! so the loader refuses at load time whatever would otherwise fail when
//! a build had already reached the step: an unknown key, a template
//! naming a slot the workflow never declared, an optional slot used
//! where it may be absent, a route target naming nothing. See
//! [`WorkflowError`] for each rule.

pub mod render;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

use crate::aigentic_skills::{hash_bytes, hash_file};
use render::Template;

/// The file that makes a folder a workflow.
pub const WORKFLOW_FILE: &str = "workflow.toml";
/// The directory under each root: `<root>/<name>/workflow.toml`.
pub const DIR: &str = "workflows";
/// A target that is an end, not a step: `done` ends the run, `ask` hands
/// over to the human (section 5).
pub const TERMINALS: &[&str] = &["done", "ask"];
/// The `filled_by` that means the runner fills the slot, not a step.
pub const RUNNER: &str = "runner";

/// Everything a workflow loader can refuse.
#[derive(Debug, thiserror::Error)]
pub enum WorkflowError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A `workflow.toml` the toml crate refused, an unknown key among
    /// the reasons.
    #[error("{path}: {message}")]
    Parse { path: PathBuf, message: String },
    /// A template's syntax: an unclosed or stray section, a section
    /// inside a section, a tag that is not a slot name.
    #[error("{path}: {message}")]
    Syntax { path: PathBuf, message: String },
    /// A template names a slot the workflow never declared.
    #[error("{path}: template names undeclared slot `{slot}`")]
    UndeclaredSlot { path: PathBuf, slot: String },
    /// An optional slot used as `{{name}}`; it may be absent at render,
    /// so it may only appear inside its own section.
    #[error("{path}: optional slot `{slot}` is used outside its own section")]
    OptionalOutsideSection { path: PathBuf, slot: String },
    /// A step's `template` is absolute or walks out of the folder.
    #[error(
        "{dir}: step `{step}` names template `{template}`, not a relative path inside the folder"
    )]
    TemplatePath {
        dir: PathBuf,
        step: String,
        template: String,
    },
    /// A step's `template` is not a file.
    #[error("{path}: step `{step}` names template `{template}`, which is not a file")]
    MissingTemplate {
        path: PathBuf,
        step: String,
        template: String,
    },
    /// A `next` or a route names no step, no named route and no terminal.
    #[error("{dir}: {from} names `{target}`, which is no step, route or terminal")]
    UnknownTarget {
        dir: PathBuf,
        from: String,
        target: String,
    },
    #[error("{dir}: duplicate step id `{id}`")]
    DuplicateStep { dir: PathBuf, id: String },
    #[error("{dir}: duplicate slot name `{name}`")]
    DuplicateSlot { dir: PathBuf, name: String },
    /// A slot's `filled_by` is neither `runner` nor a step id.
    #[error("{dir}: slot `{slot}` is filled by `{filled_by}`, which is no runner and no step")]
    UnknownFilledBy {
        dir: PathBuf,
        slot: String,
        filled_by: String,
    },
    /// A render was asked for a step this workflow does not have.
    #[error("this workflow has no step `{step}`")]
    UnknownStep { step: String },
    /// The slot map lacks a slot the template inserts.
    #[error("{path}: no value for slot `{slot}`")]
    MissingSlot { path: PathBuf, slot: String },
    /// A list, object or null used where a scalar is inserted.
    #[error("{path}: slot `{slot}` is {kind}, which has no scalar form")]
    NotScalar {
        path: PathBuf,
        slot: String,
        kind: &'static str,
    },
    /// No root holds the name.
    #[error("no workflow `{name}` in the project, user or bundled roots")]
    NotFound { name: String },
    #[error(transparent)]
    Skill(#[from] crate::aigentic_skills::SkillError),
}

/// `workflow.toml`. Every struct is `deny_unknown_fields`, so a
/// misspelled key is a load error rather than a silently ignored line.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowFile {
    pub name: String,
    pub version: u32,
    /// Per issue, in USD; `budget` fills the brief's three slots.
    pub budget: Budget,
    /// Every slot any of its templates may use, checked at load.
    #[serde(default)]
    pub slots: Vec<SlotDecl>,
    pub steps: Vec<Step>,
    /// Named step lists a route may jump to, `[routes.fix-alone]`.
    #[serde(default)]
    pub routes: BTreeMap<String, Route>,
}

/// The workflow's file shape, by the name the plan's loader uses:
/// `Workflow::load`.
pub type Workflow = WorkflowFile;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    pub trivial: f64,
    pub full: f64,
    pub max_raise: f64,
}

/// One `[[slots]]` entry: the workflow's declaration of a name its
/// templates use, so a typo in a template fails when the workflow loads,
/// not when a build reaches the step.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlotDecl {
    pub name: String,
    pub kind: SlotKind,
    #[serde(default = "yes")]
    pub required: bool,
    /// `runner`, or the step id that reports it.
    pub filled_by: String,
}

/// How a slot is rendered. `bool` renders `true`/`false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotKind {
    String,
    Bool,
}

fn yes() -> bool {
    true
}

/// One `[[steps]]` entry.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub id: String,
    /// The role the child thread runs as.
    pub role: String,
    /// The profile whose `model` fills the `model` slot.
    pub profile: String,
    /// Relative to the workflow folder.
    pub template: String,
    /// The heading the runner looks for in the step's report.
    pub marker: String,
    #[serde(default)]
    pub budget: Option<f64>,
    #[serde(default)]
    pub wall_secs: Option<u64>,
    /// The `finish_step` key this step routes on, e.g. `size`.
    #[serde(default)]
    pub route_by: Option<String>,
    /// Route value to target: a step id, a named route, `done` or `ask`.
    #[serde(default)]
    pub routes: BTreeMap<String, String>,
    /// One writing step at a time per repository.
    #[serde(default)]
    pub writes: bool,
    /// The exact checks the runner runs before the push (§6.1).
    #[serde(default)]
    pub checks: Vec<String>,
    /// The runner pushes after the checks pass.
    #[serde(default)]
    pub push: bool,
    /// Substrings refused in the step's bash calls.
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default)]
    pub next: Option<String>,
}

/// One `[routes.<name>]` entry: the steps a route jumps to, plus the
/// preconditions the runner weighs before it takes the route.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub steps: Vec<String>,
    #[serde(default)]
    pub requires: toml::value::Table,
}

/// Where a workflow came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowOrigin {
    /// `.aigentic/workflows/` in the working directory.
    Project,
    /// `~/.config/aigentic/workflows/`.
    User,
    /// `workflows/` beside the bundled skills.
    Bundled,
}

/// The three roots, in resolution order. A missing root is skipped, so a
/// `None` is as empty as a directory that is not there.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkflowRoots {
    pub project: Option<PathBuf>,
    pub user: Option<PathBuf>,
    pub bundled: Option<PathBuf>,
}

impl WorkflowRoots {
    pub fn new(project: &Path, user: &Path, bundled: &Path) -> Self {
        Self {
            project: Some(project.to_path_buf()),
            user: Some(user.to_path_buf()),
            bundled: Some(bundled.to_path_buf()),
        }
    }
}

/// A workflow that loaded: its file, where it came from, its folder, its
/// templates parsed once and its content hash for `run_started`.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedWorkflow {
    pub workflow: WorkflowFile,
    pub origin: WorkflowOrigin,
    pub dir: PathBuf,
    /// Covers `workflow.toml` and the templates it names only, so a
    /// stray file in the folder does not move it.
    pub content_hash: String,
    templates: BTreeMap<String, Template>,
}

impl WorkflowFile {
    /// The first root holding `<root>/<name>/workflow.toml` supplies the
    /// whole folder. No recursive walk: the name is the path.
    pub fn load(name: &str, roots: &WorkflowRoots) -> Result<LoadedWorkflow, WorkflowError> {
        for (root, origin) in [
            (&roots.project, WorkflowOrigin::Project),
            (&roots.user, WorkflowOrigin::User),
            (&roots.bundled, WorkflowOrigin::Bundled),
        ] {
            let Some(root) = root else { continue };
            let dir = root.join(name);
            if !dir.join(WORKFLOW_FILE).is_file() {
                continue;
            }
            return Self::load_dir(&dir, origin);
        }
        Err(WorkflowError::NotFound {
            name: name.to_string(),
        })
    }

    /// Load one folder whole: its file, its templates, its hash.
    pub fn load_dir(dir: &Path, origin: WorkflowOrigin) -> Result<LoadedWorkflow, WorkflowError> {
        let file = dir.join(WORKFLOW_FILE);
        let text = std::fs::read_to_string(&file).map_err(|source| WorkflowError::Io {
            path: file.clone(),
            source,
        })?;
        let workflow: WorkflowFile =
            toml::from_str(&text).map_err(|source| WorkflowError::Parse {
                path: file,
                message: source.to_string(),
            })?;
        workflow.validate_shape(dir)?;
        let templates = workflow.parse_templates(dir)?;
        workflow.validate_templates(&templates)?;
        let content_hash = workflow.content_hash(dir)?;
        Ok(LoadedWorkflow {
            origin,
            dir: dir.to_path_buf(),
            content_hash,
            templates,
            workflow,
        })
    }

    /// Rules that need no template text: a template path inside the
    /// folder and a file, unique step ids and slot names, a `filled_by`
    /// that is the runner or a step, and every route target naming a
    /// step, a named route or a terminal.
    fn validate_shape(&self, dir: &Path) -> Result<(), WorkflowError> {
        let mut ids: BTreeSet<&str> = BTreeSet::new();
        for step in &self.steps {
            if !ids.insert(step.id.as_str()) {
                return Err(WorkflowError::DuplicateStep {
                    dir: dir.to_path_buf(),
                    id: step.id.clone(),
                });
            }
        }
        let mut names: BTreeSet<&str> = BTreeSet::new();
        for slot in &self.slots {
            if !names.insert(slot.name.as_str()) {
                return Err(WorkflowError::DuplicateSlot {
                    dir: dir.to_path_buf(),
                    name: slot.name.clone(),
                });
            }
            if slot.filled_by != RUNNER && !ids.contains(slot.filled_by.as_str()) {
                return Err(WorkflowError::UnknownFilledBy {
                    dir: dir.to_path_buf(),
                    slot: slot.name.clone(),
                    filled_by: slot.filled_by.clone(),
                });
            }
        }
        for step in &self.steps {
            let rel = Path::new(&step.template);
            let inside =
                !rel.is_absolute() && !rel.components().any(|c| matches!(c, Component::ParentDir));
            if !inside {
                return Err(WorkflowError::TemplatePath {
                    dir: dir.to_path_buf(),
                    step: step.id.clone(),
                    template: step.template.clone(),
                });
            }
            let path = dir.join(rel);
            if !path.is_file() {
                return Err(WorkflowError::MissingTemplate {
                    path,
                    step: step.id.clone(),
                    template: step.template.clone(),
                });
            }
        }
        for step in &self.steps {
            let from = format!("step `{}`", step.id);
            if let Some(next) = &step.next {
                self.check_target(dir, &from, next, &ids)?;
            }
            for target in step.routes.values() {
                self.check_target(dir, &from, target, &ids)?;
            }
        }
        for (name, route) in &self.routes {
            let from = format!("route `{name}`");
            for target in &route.steps {
                self.check_target(dir, &from, target, &ids)?;
            }
        }
        Ok(())
    }

    fn check_target(
        &self,
        dir: &Path,
        from: &str,
        target: &str,
        ids: &BTreeSet<&str>,
    ) -> Result<(), WorkflowError> {
        let known =
            ids.contains(target) || self.routes.contains_key(target) || TERMINALS.contains(&target);
        if known {
            return Ok(());
        }
        Err(WorkflowError::UnknownTarget {
            dir: dir.to_path_buf(),
            from: from.to_string(),
            target: target.to_string(),
        })
    }

    /// Read and parse each template a step names, once per distinct
    /// path.
    fn parse_templates(&self, dir: &Path) -> Result<BTreeMap<String, Template>, WorkflowError> {
        let mut templates = BTreeMap::new();
        for step in &self.steps {
            if templates.contains_key(&step.template) {
                continue;
            }
            let path = dir.join(&step.template);
            let text = std::fs::read_to_string(&path).map_err(|source| WorkflowError::Io {
                path: path.clone(),
                source,
            })?;
            templates.insert(step.template.clone(), Template::parse(&path, &text)?);
        }
        Ok(templates)
    }

    /// Every tag checked against `[[slots]]`: an undeclared name is an
    /// error, and so is an optional slot used as `{{name}}` outside its
    /// own section, since it may be absent when the step renders.
    fn validate_templates(
        &self,
        templates: &BTreeMap<String, Template>,
    ) -> Result<(), WorkflowError> {
        let declared: BTreeMap<&str, &SlotDecl> = self
            .slots
            .iter()
            .map(|slot| (slot.name.as_str(), slot))
            .collect();
        for step in &self.steps {
            let template = templates
                .get(&step.template)
                .expect("templates were parsed in the same load");
            let mut enclosing = Vec::new();
            check_nodes(&declared, template, template.nodes(), &mut enclosing)?;
        }
        Ok(())
    }

    /// The hash recorded in `run_started`, so a run replays against the
    /// workflow it began with: `workflow.toml` plus each template a step
    /// names, de-duplicated and sorted by relative `/`-joined path, over
    /// `relpath + "\n" + hash(file) + "\n"`. No absolute paths, so the
    /// same folder hashes the same on any machine.
    fn content_hash(&self, dir: &Path) -> Result<String, WorkflowError> {
        let mut entries: BTreeSet<&str> = BTreeSet::new();
        entries.insert(WORKFLOW_FILE);
        for step in &self.steps {
            entries.insert(step.template.as_str());
        }
        let mut buf = String::new();
        for rel in entries {
            buf.push_str(rel);
            buf.push('\n');
            buf.push_str(&hash_file(&dir.join(rel))?);
            buf.push('\n');
        }
        Ok(hash_bytes(buf.as_bytes()))
    }
}

fn check_nodes(
    declared: &BTreeMap<&str, &SlotDecl>,
    template: &Template,
    nodes: &[render::Node],
    enclosing: &mut Vec<String>,
) -> Result<(), WorkflowError> {
    for node in nodes {
        match node {
            render::Node::Text(_) => {}
            render::Node::Slot(name) => {
                let slot =
                    declared
                        .get(name.as_str())
                        .ok_or_else(|| WorkflowError::UndeclaredSlot {
                            path: template.path().to_path_buf(),
                            slot: name.clone(),
                        })?;
                let inside_its_section = enclosing.iter().any(|open| open == name);
                if !slot.required && !inside_its_section {
                    return Err(WorkflowError::OptionalOutsideSection {
                        path: template.path().to_path_buf(),
                        slot: name.clone(),
                    });
                }
            }
            render::Node::Section { name, body } => {
                declared
                    .get(name.as_str())
                    .ok_or_else(|| WorkflowError::UndeclaredSlot {
                        path: template.path().to_path_buf(),
                        slot: name.clone(),
                    })?;
                enclosing.push(name.clone());
                check_nodes(declared, template, body, enclosing)?;
                enclosing.pop();
            }
        }
    }
    Ok(())
}

impl LoadedWorkflow {
    /// Render one step's template from a slot map, the same type as
    /// `StepReport.slots`. The runner builds it: its own slots merged with
    /// what a step reported, with typed report fields such as
    /// `planned_tests` rendered to text first, since a list used as
    /// `{{name}}` is an error.
    pub fn render(
        &self,
        step_id: &str,
        slots: &BTreeMap<String, serde_json::Value>,
    ) -> Result<String, WorkflowError> {
        let step = self
            .workflow
            .steps
            .iter()
            .find(|step| step.id == step_id)
            .ok_or_else(|| WorkflowError::UnknownStep {
                step: step_id.to_string(),
            })?;
        let template = self
            .templates
            .get(&step.template)
            .expect("templates were parsed in the same load");
        template.render(slots)
    }

    /// One declared slot by name.
    pub fn slot(&self, name: &str) -> Option<&SlotDecl> {
        self.workflow.slots.iter().find(|slot| slot.name == name)
    }

    /// One step by id.
    pub fn step(&self, id: &str) -> Option<&Step> {
        self.workflow.steps.iter().find(|step| step.id == id)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::fs;

    /// A valid header, every fixture starts from it.
    pub(crate) const HEAD: &str =
        "name = \"test\"\nversion = 1\n\n[budget]\ntrivial = 3.0\nfull = 10.0\nmax_raise = 2.0\n\n";

    pub(crate) fn slot(name: &str, kind: &str, filled_by: &str, required: bool) -> String {
        format!(
            "[[slots]]\nname = \"{name}\"\nkind = \"{kind}\"\nrequired = {required}\nfilled_by = \"{filled_by}\"\n\n"
        )
    }

    pub(crate) fn step(id: &str, template: &str, extra: &str) -> String {
        format!(
            "[[steps]]\nid = \"{id}\"\nrole = \"{id}\"\nprofile = \"flash\"\ntemplate = \"{template}\"\nmarker = \"## X\"\n{extra}\n"
        )
    }

    /// The valid body every refusal test starts from: three slots and one
    /// step, its `next` a terminal.
    pub(crate) fn body() -> String {
        let mut out = String::new();
        out.push_str(&slot("issue", "string", "runner", true));
        out.push_str(&slot("purpose", "string", "brief", true));
        out.push_str(&slot("flag", "string", "runner", false));
        out.push_str(&step("brief", "templates/b.md", "next = \"done\"\n"));
        out
    }

    /// The template the valid body names.
    pub(crate) const TEMPLATE: &str = "brief {{issue}}\n{{#flag}}flag{{/flag}}\n";

    /// One slot, for the fixtures that need the smallest valid file.
    pub(crate) fn one_slot() -> String {
        slot("issue", "string", "runner", true)
    }

    pub(crate) fn temp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    /// Write a workflow folder at `dir` itself (`dir` is the folder, not
    /// a root).
    pub(crate) fn write_folder(dir: &Path, toml: &str, templates: &[(&str, &str)]) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(WORKFLOW_FILE), toml).unwrap();
        for (rel, text) in templates {
            let path = dir.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
    }

    pub(crate) fn load(root: &Path, name: &str) -> Result<LoadedWorkflow, WorkflowError> {
        WorkflowFile::load(
            name,
            &WorkflowRoots {
                project: Some(root.to_path_buf()),
                user: None,
                bundled: None,
            },
        )
    }

    /// A workflow of one step `s` over the given slot declarations,
    /// loaded — the shape the render tests need.
    pub(crate) fn one_step(root: &Path, slots: &str, template: &str) -> LoadedWorkflow {
        let toml = format!(
            "{HEAD}{slots}{}",
            step("s", "templates/x.md", "next = \"done\"\n")
        );
        write_folder(&root.join("test"), &toml, &[("templates/x.md", template)]);
        load(root, "test").expect("the fixture loads")
    }

    /// T15 — the bundled `workflows/build` is checked in and loads: its deny
    /// list is the issue's eight entries, and both its templates render
    /// from a slot map of every required slot (no optional one) and again
    /// with every optional slot present, leaving no tag and no run of
    /// blank lines behind.
    #[test]
    fn bundled_build_workflow_loads_and_renders() {
        let bundled = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("workflows");
        let loaded = WorkflowFile::load(
            "build",
            &WorkflowRoots {
                project: None,
                user: None,
                bundled: Some(bundled),
            },
        )
        .expect("the bundled build workflow loads");
        assert_eq!(loaded.origin, WorkflowOrigin::Bundled);
        assert_eq!(loaded.workflow.name, "build");
        assert_eq!(loaded.workflow.version, 1);
        assert_eq!(
            loaded.workflow.budget,
            Budget {
                trivial: 3.0,
                full: 10.0,
                max_raise: 2.0
            }
        );

        // The deny list is `## Templates`'s eight entries, verbatim.
        let step = loaded.step("implement-alone").expect("the step is there");
        assert_eq!(
            step.deny,
            [
                "git push",
                "git rebase",
                "git reset",
                "git checkout --",
                "git stash",
                "git clean",
                "git add -A",
                "copy .env",
            ]
        );

        let mut slots: BTreeMap<String, serde_json::Value> = loaded
            .workflow
            .slots
            .iter()
            .filter(|slot| slot.required)
            .map(|slot| {
                let value = match slot.kind {
                    SlotKind::Bool => serde_json::Value::Bool(true),
                    SlotKind::String => serde_json::Value::String(format!("<{}>", slot.name)),
                };
                (slot.name.clone(), value)
            })
            .collect();

        for step_id in ["brief", "implement-alone"] {
            let out = loaded.render(step_id, &slots).expect("it renders");
            assert!(!out.contains("{{"), "{step_id} left a tag behind");
            assert!(!out.contains("\n\n\n"), "{step_id} left blank lines");
        }

        // And with every optional slot present: the sections appear, and
        // still nothing is doubled.
        for (name, value) in [
            ("reference_check", serde_json::Value::from("<check>")),
            ("ui", serde_json::Value::Bool(true)),
            ("check_failures", serde_json::Value::from("<failed>")),
        ] {
            slots.insert(name.to_string(), value);
        }
        for step_id in ["brief", "implement-alone"] {
            let out = loaded.render(step_id, &slots).expect("it renders");
            assert!(!out.contains("{{"), "{step_id} left a tag behind");
            assert!(!out.contains("\n\n\n"), "{step_id} left blank lines");
        }
        let out = loaded.render("implement-alone", &slots).unwrap();
        assert!(out.contains("## Reference check"), "the section is kept");
        assert!(out.contains("## Sent back by the runner"), "kept");
        assert!(out.contains("<failed>"), "the slot inside it is filled");
    }

    /// T1 — an unknown key is refused, not ignored.
    #[test]
    fn unknown_key_is_refused() {
        let root = temp();
        write_folder(
            &root.path().join("test"),
            &format!("{HEAD}nope = 1\n\n{}", body()),
            &[("templates/b.md", TEMPLATE)],
        );
        assert!(matches!(
            load(root.path(), "test"),
            Err(WorkflowError::Parse { .. })
        ));
    }

    /// T2 — a step whose template is not on disk.
    #[test]
    fn missing_template_file_is_refused() {
        let root = temp();
        write_folder(&root.path().join("test"), &format!("{HEAD}{}", body()), &[]);
        assert!(matches!(
            load(root.path(), "test"),
            Err(WorkflowError::MissingTemplate { .. })
        ));
    }

    /// T3 — a template naming a slot the workflow never declared.
    #[test]
    fn undeclared_slot_is_refused() {
        let root = temp();
        write_folder(
            &root.path().join("test"),
            &format!("{HEAD}{}", body()),
            &[("templates/b.md", "{{nope}}\n")],
        );
        assert!(matches!(
            load(root.path(), "test"),
            Err(WorkflowError::UndeclaredSlot { slot, .. }) if slot == "nope"
        ));
    }

    /// T4 — an optional slot used as `{{name}}` outside its own section:
    /// it may be absent at render, so it may not be inserted bare.
    #[test]
    fn optional_slot_outside_its_section_is_refused() {
        let root = temp();
        write_folder(
            &root.path().join("test"),
            &format!("{HEAD}{}", body()),
            &[("templates/b.md", "{{flag}}\n")],
        );
        assert!(matches!(
            load(root.path(), "test"),
            Err(WorkflowError::OptionalOutsideSection { slot, .. }) if slot == "flag"
        ));
    }

    /// T5 — a section never closed.
    #[test]
    fn unbalanced_section_is_refused() {
        let root = temp();
        write_folder(
            &root.path().join("test"),
            &format!("{HEAD}{}", body()),
            &[("templates/b.md", "{{#flag}}x\n")],
        );
        assert!(matches!(
            load(root.path(), "test"),
            Err(WorkflowError::Syntax { .. })
        ));
    }

    /// T6 — a section inside a section.
    #[test]
    fn nested_section_is_refused() {
        let root = temp();
        write_folder(
            &root.path().join("test"),
            &format!("{HEAD}{}", body()),
            &[("templates/b.md", "{{#flag}}{{#flag}}x{{/flag}}{{/flag}}\n")],
        );
        assert!(matches!(
            load(root.path(), "test"),
            Err(WorkflowError::Syntax { .. })
        ));
    }

    /// T7 — a close with no open section.
    #[test]
    fn stray_close_is_refused() {
        let root = temp();
        write_folder(
            &root.path().join("test"),
            &format!("{HEAD}{}", body()),
            &[("templates/b.md", "x{{/flag}}\n")],
        );
        assert!(matches!(
            load(root.path(), "test"),
            Err(WorkflowError::Syntax { .. })
        ));
    }

    /// T8 — a `next` (and, in the same rule, a route value or a
    /// `[routes.*].steps` entry) naming no step, route or terminal.
    #[test]
    fn unknown_route_target_is_refused() {
        let root = temp();
        write_folder(
            &root.path().join("test"),
            &format!(
                "{HEAD}{}{}",
                one_slot(),
                step("brief", "templates/b.md", "next = \"nowhere\"\n")
            ),
            &[("templates/b.md", "x\n")],
        );
        assert!(matches!(
            load(root.path(), "test"),
            Err(WorkflowError::UnknownTarget { target, .. }) if target == "nowhere"
        ));
    }

    /// T9 — two steps with the same id.
    #[test]
    fn duplicate_step_id_is_refused() {
        let root = temp();
        write_folder(
            &root.path().join("test"),
            &format!(
                "{HEAD}{}{}",
                body(),
                step("brief", "templates/b.md", "next = \"done\"\n")
            ),
            &[("templates/b.md", TEMPLATE)],
        );
        assert!(matches!(
            load(root.path(), "test"),
            Err(WorkflowError::DuplicateStep { id, .. }) if id == "brief"
        ));
    }

    /// T10 — a `filled_by` that is neither the runner nor a step.
    #[test]
    fn unknown_filled_by_is_refused() {
        let root = temp();
        write_folder(
            &root.path().join("test"),
            &format!(
                "{HEAD}{}{}",
                slot("issue", "string", "nobody", true),
                step("brief", "templates/b.md", "next = \"done\"\n")
            ),
            &[("templates/b.md", "x\n")],
        );
        assert!(matches!(
            load(root.path(), "test"),
            Err(WorkflowError::UnknownFilledBy { filled_by, .. }) if filled_by == "nobody"
        ));
    }

    /// The path rule behind T2: a template must be a relative path inside
    /// the folder, so a step cannot reach out of it.
    #[test]
    fn absolute_or_parent_template_path_is_refused() {
        let root = temp();
        write_folder(
            &root.path().join("test"),
            &format!(
                "{HEAD}{}{}",
                slot("issue", "string", "runner", true),
                step("brief", "../outside.md", "next = \"done\"\n")
            ),
            &[("templates/b.md", "x\n")],
        );
        assert!(matches!(
            load(root.path(), "test"),
            Err(WorkflowError::TemplatePath { template, .. }) if template == "../outside.md"
        ));
    }

    /// Two declarations of one slot name: nothing decides between them.
    #[test]
    fn duplicate_slot_name_is_refused() {
        let root = temp();
        write_folder(
            &root.path().join("test"),
            &format!(
                "{HEAD}{}{}{}",
                slot("issue", "string", "runner", true),
                slot("issue", "bool", "runner", true),
                step("brief", "templates/b.md", "next = \"done\"\n")
            ),
            &[("templates/b.md", "{{issue}}\n")],
        );
        assert!(matches!(
            load(root.path(), "test"),
            Err(WorkflowError::DuplicateSlot { name, .. }) if name == "issue"
        ));
    }

    /// T11 — one name in three roots: project wins, then user, then
    /// bundled; a root with no such folder is skipped, and a name in no
    /// root is the only error.
    #[test]
    fn resolution_prefers_project_then_user_then_bundled() {
        let root = temp();
        let project = root.path().join("project");
        let user = root.path().join("user");
        let bundled = root.path().join("bundled");
        let toml = format!("{HEAD}{}", body());
        for workflows in [&project, &user, &bundled] {
            write_folder(
                &workflows.join("test"),
                &toml,
                &[("templates/b.md", TEMPLATE)],
            );
        }
        let roots = WorkflowRoots::new(&project, &user, &bundled);
        assert_eq!(
            WorkflowFile::load("test", &roots).unwrap().origin,
            WorkflowOrigin::Project
        );
        fs::remove_dir_all(project.join("test")).unwrap();
        assert_eq!(
            WorkflowFile::load("test", &roots).unwrap().origin,
            WorkflowOrigin::User
        );
        fs::remove_dir_all(user.join("test")).unwrap();
        assert_eq!(
            WorkflowFile::load("test", &roots).unwrap().origin,
            WorkflowOrigin::Bundled
        );
        fs::remove_dir_all(bundled.join("test")).unwrap();
        assert!(matches!(
            WorkflowFile::load("test", &roots),
            Err(WorkflowError::NotFound { .. })
        ));
        // A root that is not there at all is empty, not an error.
        let none = WorkflowRoots {
            project: None,
            user: None,
            bundled: None,
        };
        assert!(matches!(
            WorkflowFile::load("test", &none),
            Err(WorkflowError::NotFound { .. })
        ));
    }

    /// T13 — the content hash covers the workflow, not the folder: two
    /// copies of one folder hash equal, a changed template moves the
    /// hash, and an unrelated file dropped into the folder does not.
    #[test]
    fn content_hash_is_stable_and_ignores_unrelated_files() {
        let root = temp();
        let toml = format!("{HEAD}{}", body());
        let a = root.path().join("a").join("test");
        let b = root.path().join("b").join("test");
        write_folder(&a, &toml, &[("templates/b.md", TEMPLATE)]);
        write_folder(&b, &toml, &[("templates/b.md", TEMPLATE)]);
        let hash = |dir: &Path| WorkflowFile::load_dir(dir, WorkflowOrigin::Project).unwrap();
        let first = hash(&a);
        assert_eq!(first.content_hash, hash(&b).content_hash);

        // One flipped byte in a template is a different workflow.
        fs::write(b.join("templates/b.md"), "brief {{issue}}\n").unwrap();
        assert_ne!(first.content_hash, hash(&b).content_hash);

        // A file no template names is not part of the workflow.
        fs::write(a.join("notes.txt"), "mine\n").unwrap();
        assert_eq!(first.content_hash, hash(&a).content_hash);
    }
}
