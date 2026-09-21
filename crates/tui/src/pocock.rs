//! `aigentic project setup`: render the files upstream's engineering
//! skills read from `[pocock]` in `aigentic.toml`, byte for byte what
//! `setup-matt-pocock-skills` writes for the same answers, so the vendored
//! skills find what they expect without running the interactive skill.
//! See `docs/PLAN-phase4.md` section 3 and done-when 5.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use aigentic_runtime::Project;
use aigentic_runtime::project::{DOT_DIR, INSTRUCTIONS_FILE, PocockSection};
use anyhow::{Context, bail};

use crate::pocock_templates::{
    DOMAIN, ISSUE_TRACKER_GITHUB, ISSUE_TRACKER_GITLAB, ISSUE_TRACKER_LOCAL, TRIAGE_LABELS,
};

/// The five canonical triage roles, in the table's order, with the
/// meaning column upstream writes.
pub const TRIAGE_ROLES: [(&str, &str); 5] = [
    ("needs-triage", "Maintainer needs to evaluate this issue"),
    ("needs-info", "Waiting on reporter for more information"),
    ("ready-for-agent", "Fully specified, ready for an AFK agent"),
    ("ready-for-human", "Requires human implementation"),
    ("wontfix", "Will not be actioned"),
];

const BLOCK_HEADING: &str = "## Agent skills";

/// One file `setup` writes, relative to the project root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub path: PathBuf,
    pub text: String,
}

/// The files for a `[pocock]` section. `triage` is whether the `triage`
/// skill is enabled: upstream writes the labels file only then, or here
/// also when overrides are given.
pub fn render(pocock: &PocockSection, triage: bool) -> anyhow::Result<Vec<Rendered>> {
    let docs = pocock.docs_dir.as_deref().unwrap_or("docs");
    let agents = Path::new(docs).join("agents");
    let tracker = match pocock.issue_tracker.as_str() {
        "github" => with_flag(ISSUE_TRACKER_GITHUB, "PRs", pocock.prs_as_requests),
        "gitlab" => with_flag(ISSUE_TRACKER_GITLAB, "MRs", pocock.prs_as_requests),
        "local" => ISSUE_TRACKER_LOCAL.to_owned(),
        other => bail!(
            "[pocock] issue_tracker must be \"github\", \"gitlab\" or \"local\", got {other:?}"
        ),
    };
    for role in pocock.triage_labels.keys() {
        if !TRIAGE_ROLES.iter().any(|(r, _)| r == role) {
            bail!(
                "[pocock] triage_labels: unknown role {role:?}; the roles are {}",
                TRIAGE_ROLES
                    .iter()
                    .map(|(r, _)| *r)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    let mut files = vec![
        Rendered {
            path: agents.join("issue-tracker.md"),
            text: tracker,
        },
        Rendered {
            path: agents.join("domain.md"),
            text: DOMAIN.to_owned(),
        },
    ];
    if triage || !pocock.triage_labels.is_empty() {
        files.insert(
            1,
            Rendered {
                path: agents.join("triage-labels.md"),
                text: triage_labels(&pocock.triage_labels),
            },
        );
    }
    Ok(files)
}

/// The request-surface flag line, `no` by default as upstream leaves it.
fn with_flag(template: &str, what: &str, on: bool) -> String {
    let off = format!("**{what} as a request surface: no.**");
    if on {
        template.replace(&off, &format!("**{what} as a request surface: yes.**"))
    } else {
        template.to_owned()
    }
}

/// Upstream's labels file with the right-hand column from the overrides;
/// no overrides reproduces it byte for byte. The table is re-padded the
/// way upstream's formatter pads it, so a long label still lines up.
pub fn triage_labels(overrides: &BTreeMap<String, String>) -> String {
    let (head, _) = TRIAGE_LABELS
        .split_once("| Label in mattpocock/skills")
        .expect("template has the table");
    let (_, tail) = TRIAGE_LABELS
        .split_once("| Will not be actioned                     |\n")
        .expect("template has the last row");
    let header = [
        "Label in mattpocock/skills",
        "Label in our tracker",
        "Meaning",
    ];
    let rows: Vec<[String; 3]> = TRIAGE_ROLES
        .iter()
        .map(|(role, meaning)| {
            let ours = overrides.get(*role).map_or(*role, String::as_str);
            [
                format!("`{role}`"),
                format!("`{ours}`"),
                (*meaning).to_owned(),
            ]
        })
        .collect();
    let widths: [usize; 3] = std::array::from_fn(|i| {
        rows.iter()
            .map(|r| r[i].chars().count())
            .chain([header[i].len()])
            .max()
            .unwrap_or(0)
    });
    let line = |cells: [&str; 3]| {
        format!(
            "| {:<w0$} | {:<w1$} | {:<w2$} |\n",
            cells[0],
            cells[1],
            cells[2],
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2]
        )
    };
    let mut out = String::from(head);
    out.push_str(&line(header));
    out.push_str(&format!(
        "| {} | {} | {} |\n",
        "-".repeat(widths[0]),
        "-".repeat(widths[1]),
        "-".repeat(widths[2])
    ));
    for r in &rows {
        out.push_str(&line([&r[0], &r[1], &r[2]]));
    }
    out.push_str(tail);
    out
}

/// The `## Agent skills` block upstream adds to the instructions file,
/// pointing at the rendered files. The labels sub-block appears only when
/// the labels file is written.
pub fn agent_skills_block(pocock: &PocockSection, files: &[Rendered]) -> String {
    let path = |name: &str| {
        files
            .iter()
            .find(|f| f.path.file_name().is_some_and(|n| n == name))
            .map(|f| f.path.display().to_string())
    };
    let tracker = match pocock.issue_tracker.as_str() {
        "github" => "Issues live in this repo's GitHub Issues, through the `gh` CLI",
        "gitlab" => "Issues live in this repo's GitLab Issues, through the `glab` CLI",
        _ => "Issues live as markdown files under `.scratch/<feature>/` in this repo",
    };
    let mut out = format!(
        "{BLOCK_HEADING}\n\n### Issue tracker\n\n{tracker}. See `{}`.\n",
        path("issue-tracker.md").unwrap_or_default()
    );
    if let Some(labels) = path("triage-labels.md") {
        let vocabulary = if pocock.triage_labels.is_empty() {
            "The five canonical triage labels, each named after its role".to_owned()
        } else {
            format!(
                "The five canonical triage roles, with {} mapped to this tracker's own labels",
                pocock
                    .triage_labels
                    .iter()
                    .map(|(role, ours)| format!("`{role}` as `{ours}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        out.push_str(&format!(
            "\n### Triage labels\n\n{vocabulary}. See `{labels}`.\n"
        ));
    }
    out.push_str(&format!(
        "\n### Domain docs\n\nSingle-context: `CONTEXT.md` at the root and ADRs under `{}/adr/`. See `{}`.\n",
        pocock.docs_dir.as_deref().unwrap_or("docs"),
        path("domain.md").unwrap_or_default()
    ));
    out
}

/// `existing` with the block replaced in place when a `## Agent skills`
/// heading is present, else appended after one blank line. Other sections
/// are untouched.
pub fn apply_block(existing: &str, block: &str) -> String {
    let block = block.trim_end();
    let Some(start) = find_heading(existing) else {
        if existing.trim().is_empty() {
            return format!("{block}\n");
        }
        let sep = if existing.ends_with("\n\n") {
            ""
        } else if existing.ends_with('\n') {
            "\n"
        } else {
            "\n\n"
        };
        return format!("{existing}{sep}{block}\n");
    };
    // The block runs to the next `## ` heading (not `###`) or the end.
    let rest = &existing[start + BLOCK_HEADING.len()..];
    let end = rest
        .match_indices("\n## ")
        .map(|(i, _)| start + BLOCK_HEADING.len() + i + 1)
        .next()
        .unwrap_or(existing.len());
    let mut out = String::with_capacity(existing.len() + block.len());
    out.push_str(&existing[..start]);
    out.push_str(block);
    out.push('\n');
    if end < existing.len() {
        out.push('\n');
        out.push_str(&existing[end..]);
    }
    out
}

fn find_heading(text: &str) -> Option<usize> {
    text.match_indices(BLOCK_HEADING)
        .map(|(i, _)| i)
        .find(|&i| {
            (i == 0 || text[..i].ends_with('\n')) && {
                let after = &text[i + BLOCK_HEADING.len()..];
                after.is_empty() || after.starts_with('\n') || after.starts_with('\r')
            }
        })
}

/// What `setup` did to one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Written,
    Unchanged,
}

/// Write the files under the project root and put the block into the
/// project's instructions file: `.aigentic/instructions.md` if present,
/// else `AGENTS.md`, created when neither exists. Returns each path with
/// what happened to it.
pub fn setup(project: &Project) -> anyhow::Result<Vec<(PathBuf, Outcome)>> {
    let Some(pocock) = project.file.pocock.as_ref() else {
        bail!(
            "no [pocock] section in {}; add one with issue_tracker = \"github\" | \"gitlab\" | \"local\"",
            project
                .root
                .join(aigentic_runtime::project::FILE_NAME)
                .display()
        );
    };
    let triage = project.file.skills.enabled.iter().any(|s| s == "triage");
    let files = render(pocock, triage)?;
    let mut done = Vec::new();
    for f in &files {
        let path = project.root.join(&f.path);
        done.push((f.path.clone(), write_if_changed(&path, &f.text)?));
    }
    let block = agent_skills_block(pocock, &files);
    let ours = project.root.join(DOT_DIR).join(INSTRUCTIONS_FILE);
    let instructions = if ours.is_file() {
        ours
    } else {
        project.root.join("AGENTS.md")
    };
    let existing = match std::fs::read_to_string(&instructions) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", instructions.display())),
    };
    let rel = instructions
        .strip_prefix(&project.root)
        .map_or_else(|_| instructions.clone(), Path::to_path_buf);
    done.push((
        rel,
        write_if_changed(&instructions, &apply_block(&existing, &block))?,
    ));
    Ok(done)
}

fn write_if_changed(path: &Path, text: &str) -> anyhow::Result<Outcome> {
    if std::fs::read_to_string(path).is_ok_and(|t| t == text) {
        return Ok(Outcome::Unchanged);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(Outcome::Written)
}

/// `aigentic project setup`.
pub fn run(project: Option<&Project>) -> anyhow::Result<i32> {
    let Some(project) = project else {
        bail!("no aigentic.toml here or above; run `aigentic project init` first");
    };
    for (path, outcome) in setup(project)? {
        let what = match outcome {
            Outcome::Written => "wrote",
            Outcome::Unchanged => "unchanged",
        };
        println!("{what:<9} {}", path.display());
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aigentic_runtime::project::FILE_NAME;

    fn upstream(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../skills/pocock/engineering/setup-matt-pocock-skills")
            .join(name);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    fn section(tracker: &str) -> PocockSection {
        PocockSection {
            issue_tracker: tracker.into(),
            triage_labels: BTreeMap::new(),
            docs_dir: None,
            prs_as_requests: false,
        }
    }

    #[test]
    fn the_snapshot_equals_the_vendored_upstream_templates() {
        assert_eq!(ISSUE_TRACKER_GITHUB, upstream("issue-tracker-github.md"));
        assert_eq!(ISSUE_TRACKER_GITLAB, upstream("issue-tracker-gitlab.md"));
        assert_eq!(ISSUE_TRACKER_LOCAL, upstream("issue-tracker-local.md"));
        assert_eq!(TRIAGE_LABELS, upstream("triage-labels.md"));
        assert_eq!(DOMAIN, upstream("domain.md"));
    }

    #[test]
    fn default_answers_render_upstreams_files_byte_for_byte() {
        for (tracker, template) in [
            ("github", ISSUE_TRACKER_GITHUB),
            ("gitlab", ISSUE_TRACKER_GITLAB),
            ("local", ISSUE_TRACKER_LOCAL),
        ] {
            let files = render(&section(tracker), true).unwrap();
            let paths: Vec<String> = files.iter().map(|f| f.path.display().to_string()).collect();
            assert_eq!(
                paths,
                vec![
                    "docs/agents/issue-tracker.md",
                    "docs/agents/triage-labels.md",
                    "docs/agents/domain.md"
                ]
            );
            assert_eq!(files[0].text, template, "{tracker}");
            assert_eq!(files[1].text, TRIAGE_LABELS);
            assert_eq!(files[2].text, DOMAIN);
        }
        let files = render(&section("github"), false).unwrap();
        assert_eq!(files.len(), 2, "no labels file without the triage skill");
        assert!(render(&section("jira"), false).is_err());
    }

    #[test]
    fn overrides_flag_and_docs_dir_change_only_what_they_name() {
        let mut p = section("github");
        p.docs_dir = Some("documentation".into());
        p.prs_as_requests = true;
        p.triage_labels
            .insert("needs-triage".into(), "bug:triage-please".into());
        let files = render(&p, false).unwrap();
        assert_eq!(
            files[0].path,
            PathBuf::from("documentation/agents/issue-tracker.md")
        );
        assert!(files[0].text.contains("**PRs as a request surface: yes.**"));
        assert_eq!(
            files[0].text.len(),
            ISSUE_TRACKER_GITHUB.len() + 1,
            "only the flag word changed"
        );
        let labels = &files[1].text;
        assert!(labels.contains("| `needs-triage`             | `bug:triage-please`  | Maintainer needs to evaluate this issue  |\n"), "{labels}");
        assert!(labels.contains("| `wontfix`                  | `wontfix`            | Will not be actioned                     |\n"), "{labels}");
        assert!(labels.ends_with("whatever vocabulary you actually use.\n"));
        p.triage_labels.insert("bogus".into(), "x".into());
        assert!(render(&p, false).is_err());
    }

    #[test]
    fn the_block_is_appended_once_and_replaced_in_place() {
        let files = render(&section("local"), true).unwrap();
        let block = agent_skills_block(&section("local"), &files);
        assert!(
            block.starts_with(
                "## Agent skills\n\n### Issue tracker\n\nIssues live as markdown files"
            ),
            "{block}"
        );
        assert!(block.contains("### Triage labels\n\nThe five canonical triage labels"));
        assert!(block.contains("See `docs/agents/domain.md`."));

        let fresh = apply_block("", &block);
        assert_eq!(fresh, format!("{}\n", block.trim_end()));
        let appended = apply_block("# Rules\n\nBe brief.\n", &block);
        assert_eq!(
            appended,
            format!("# Rules\n\nBe brief.\n\n{}\n", block.trim_end())
        );
        assert_eq!(apply_block(&appended, &block), appended, "idempotent");

        let other = agent_skills_block(
            &section("github"),
            &render(&section("github"), false).unwrap(),
        );
        let replaced = apply_block(&format!("{appended}\n## Later\n\nkept.\n"), &other);
        assert_eq!(
            replaced,
            format!(
                "# Rules\n\nBe brief.\n\n{}\n\n## Later\n\nkept.\n",
                other.trim_end()
            )
        );
        assert!(!replaced.contains("Triage labels"));
        assert_eq!(replaced.matches("## Agent skills").count(), 1);
    }

    #[test]
    fn setup_writes_the_files_and_the_block_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            "[project]\nname = \"p\"\n[skills]\nenabled = [\"triage\"]\n[pocock]\nissue_tracker = \"github\"\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "# p\n").unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "vendor").unwrap();
        let project = Project::open_root(dir.path()).unwrap();
        let done = setup(&project).unwrap();
        assert_eq!(
            done,
            vec![
                (
                    PathBuf::from("docs/agents/issue-tracker.md"),
                    Outcome::Written
                ),
                (
                    PathBuf::from("docs/agents/triage-labels.md"),
                    Outcome::Written
                ),
                (PathBuf::from("docs/agents/domain.md"), Outcome::Written),
                (PathBuf::from("AGENTS.md"), Outcome::Written),
            ]
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("docs/agents/issue-tracker.md")).unwrap(),
            ISSUE_TRACKER_GITHUB
        );
        let agents = std::fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();
        assert!(agents.starts_with("# p\n\n## Agent skills\n"), "{agents}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("CLAUDE.md")).unwrap(),
            "vendor",
            "another product's file is never touched"
        );
        let again = setup(&project).unwrap();
        assert!(
            again.iter().all(|(_, o)| *o == Outcome::Unchanged),
            "{again:?}"
        );

        // With `.aigentic/instructions.md` present the block goes there.
        let ours = dir.path().join(".aigentic");
        std::fs::create_dir_all(&ours).unwrap();
        std::fs::write(ours.join("instructions.md"), "ours\n").unwrap();
        let done = setup(&project).unwrap();
        assert_eq!(
            done.last().unwrap(),
            &(PathBuf::from(".aigentic/instructions.md"), Outcome::Written)
        );
        assert!(
            std::fs::read_to_string(ours.join("instructions.md"))
                .unwrap()
                .contains("## Agent skills")
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("AGENTS.md")).unwrap(),
            agents
        );
    }

    #[test]
    fn setup_needs_a_pocock_section() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), "[project]\nname = \"p\"\n").unwrap();
        let project = Project::open_root(dir.path()).unwrap();
        let err = setup(&project).unwrap_err().to_string();
        assert!(err.contains("no [pocock] section"), "{err}");
        assert!(run(None).is_err());
    }
}
