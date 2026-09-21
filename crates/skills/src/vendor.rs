//! Building lock entries for a vendored root and rendering the static
//! check's findings for a human. `aigentic skills vendor | check` (phase 3
//! step 9) call these; the snapshot test in `tests/vendored.rs` keeps
//! `skills.lock.toml` and `docs/skills-review.md` in step with the files.

use std::path::Path;

use crate::{
    Finding, FindingKind, LockEntry, Lockfile, Manifest, Origin, PATTERN_VERSION, SkillError,
    check, tools_referenced, walk_root,
};

/// Lock entries for every skill under `root`, all `Pending`, with `path`
/// written as `prefix/<folder relative to root>`. Existing entries in
/// `into` keep their `review` and `requires` when the hashes are unchanged.
pub fn lock_root(
    root: &Path,
    prefix: &str,
    source: &str,
    commit: &str,
    into: &mut Lockfile,
) -> Result<Vec<Manifest>, SkillError> {
    let manifests = walk_root(root, Origin::Bundled)?;
    for m in &manifests {
        let rel = m
            .path
            .strip_prefix(root)
            .expect("walked under root")
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        let path = if prefix.is_empty() {
            rel
        } else {
            format!("{prefix}/{rel}")
        };
        let mut entry = LockEntry::from_manifest(m, path, source, commit)?;
        if let Some(old) = into.get(&m.name)
            && old.sha256 == entry.sha256
            && old.files == entry.files
        {
            entry.review = old.review.clone();
            entry.requires = old.requires.clone();
        }
        into.upsert(entry);
    }
    Ok(manifests)
}

/// `docs/skills-review.md`: one section per skill in name order with its
/// findings grouped by kind, so a reviewer can read it top to bottom and
/// set `review` in the lock. Skills with no findings are listed at the end.
pub fn render_review(manifests: &[Manifest], lock: &Lockfile) -> String {
    let mut out = String::new();
    out.push_str("# Skills review\n\n");
    out.push_str(&format!(
        "Findings from the static check (pattern version {PATTERN_VERSION}) over the vendored \
         set. Generated; regenerate with `AIGENTIC_REGEN=1 cargo test -p aigentic-skills \
         --test vendored`. Findings are for a reviewer, not verdicts: a `Pending` skill with \
         findings blocks `aigentic skills check`, and a human clears it by setting `review` in \
         `skills.lock.toml`. Tool references are the seed for `requires`.\n\n"
    ));
    let mut sorted: Vec<&Manifest> = manifests.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    let mut clean = Vec::new();
    for m in sorted {
        let findings = check(m);
        if findings.is_empty() {
            clean.push(m.name.as_str());
            continue;
        }
        let entry = lock.get(&m.name);
        out.push_str(&format!("## {}\n\n", m.name));
        out.push_str(&format!(
            "{} · {} · `{}`\n\n",
            match m.invocation {
                crate::Invocation::User => "user-invoked",
                crate::Invocation::Model => "model-invoked",
            },
            entry.map_or("unlocked".to_owned(), |e| match &e.review {
                crate::Review::Pending => "review pending".to_owned(),
                crate::Review::Accepted { by, on } => format!("accepted by {by} on {on}"),
                crate::Review::Rejected { by, on } => format!("rejected by {by} on {on}"),
            }),
            entry.map_or_else(|| m.path.display().to_string(), |e| e.path.clone())
        ));
        let tools = tools_referenced(&findings);
        if !tools.is_empty() {
            out.push_str(&format!("Tools referenced: {}\n\n", tools.join(", ")));
        }
        for kind in [
            FindingKind::IgnoreRules,
            FindingKind::PermissionWidening,
            FindingKind::ShellInScript,
            FindingKind::Url,
            FindingKind::ToolReference,
        ] {
            let of_kind: Vec<&Finding> = findings.iter().filter(|f| f.kind == kind).collect();
            if of_kind.is_empty() {
                continue;
            }
            out.push_str(&format!("{}:\n\n", kind_title(kind)));
            for f in of_kind {
                out.push_str(&format!(
                    "- `{}:{}` {}\n",
                    f.file.display(),
                    f.line,
                    f.text.replace('`', "'")
                ));
            }
            out.push('\n');
        }
    }
    if !clean.is_empty() {
        out.push_str("## No findings\n\n");
        for name in clean {
            out.push_str(&format!("- {name}\n"));
        }
    }
    out.trim_end().to_owned() + "\n"
}

fn kind_title(kind: FindingKind) -> &'static str {
    match kind {
        FindingKind::IgnoreRules => "Asks to ignore rules",
        FindingKind::PermissionWidening => "Widens permissions",
        FindingKind::ShellInScript => "Scripts and shell",
        FindingKind::Url => "URLs",
        FindingKind::ToolReference => "Tool references",
    }
}

/// `skills check`'s exit rule: any `Pending` entry with findings blocks.
pub fn blocking(manifests: &[Manifest], lock: &Lockfile) -> Vec<String> {
    let mut names: Vec<String> = manifests
        .iter()
        .filter(|m| lock.get(&m.name).is_none_or(|e| e.review.is_pending()))
        .filter(|m| !check(m).is_empty())
        .map(|m| m.name.clone())
        .collect();
    names.sort();
    names
}
