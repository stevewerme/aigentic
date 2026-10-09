//! The projects block (issue #81): what tells the model which projects
//! exist and which workspace each is in, so a message can be proposed
//! for the right one (#7). The set comes from the daemon — only it knows
//! every project — and the rendering here is pure, so the lines are
//! testable without a daemon: `ThreadTable` gathers [`Listed`] rows and
//! this turns them into the text.

use std::path::{Path, PathBuf};

/// One project as the listing needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub name: String,
    /// The root as stored: `~/…` shortening happens only here, in
    /// [`projects_listing`], so a root written while `HOME` was unset
    /// stays literal.
    pub root: PathBuf,
    /// The workspace naming it, `workspace_of`'s first match; `None`
    /// when no workspace does.
    pub workspace: Option<String>,
    /// The brief's one-liner (issue #123): its first non-empty line,
    /// hashes and spaces stripped, cut at `ONE_LINE_CAP` characters.
    /// `None` is no brief, and no ` — ` on the row, which then reads as
    /// today.
    pub one_line: Option<String>,
}

/// The projects in reach, as one system block, or `None` when there is
/// nothing to list — no block at all, so a thread with no projects keeps
/// the prefix it had.
///
/// `all` are the projects in reach (rendered in name order), `current`
/// is the thread's own project, named from itself so line 1 is right
/// even when the thread's creator has no role there, and `home` is the
/// value a root under it is shown as `~` against.
///
/// A row of this workspace carries its brief's one-liner after its root
/// (issue #123). Other workspaces stay names only, and a project with no
/// brief reads exactly as it did before the brief existed.
pub fn projects_listing(
    all: &[Listed],
    current: Option<&Listed>,
    home: Option<&Path>,
) -> Option<String> {
    if all.is_empty() {
        return None;
    }
    let mut rows: Vec<&Listed> = all.iter().collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));

    let mut lines = vec![match current {
        Some(c) => match &c.workspace {
            Some(w) => format!("Projects in reach. This thread is in {} ({w}).", c.name),
            None => format!(
                "Projects in reach. This thread is in {} (no workspace).",
                c.name
            ),
        },
        None => "Projects in reach. This thread is in no project.".to_owned(),
    }];

    // Where the thread is, in the listing's terms: `None` is no current
    // project (no line 2 at all), `Some(None)` is a project in no
    // workspace, which line 2 then carries itself.
    let here: Option<Option<&str>> = current.map(|c| c.workspace.as_deref());

    // Line 2: the current project's workspace, each project with its
    // root and the current one marked. It covers the no-workspace
    // projects when the thread is in one of those, and line 4 is then
    // left out.
    if let Some(ws) = here {
        let side: Vec<&Listed> = rows
            .iter()
            .copied()
            .filter(|p| p.workspace.as_deref() == ws)
            .collect();
        if !side.is_empty() {
            let label = ws.unwrap_or("In no workspace");
            let body = side
                .iter()
                .map(|p| {
                    let mark = if Some(p.name.as_str()) == current.map(|c| c.name.as_str()) {
                        " (here)"
                    } else {
                        ""
                    };
                    let line = match &p.one_line {
                        Some(one) => format!(" — {one}"),
                        None => String::new(),
                    };
                    format!("{}{mark} {}{line}", p.name, shown_root(&p.root, home))
                })
                .collect::<Vec<_>>()
                .join(" · ");
            lines.push(format!("{label}: {body}"));
        }
    }

    // Line 3: every other workspace, its project names in name order.
    let mut workspaces: Vec<(&str, Vec<&str>)> = Vec::new();
    for p in &rows {
        let Some(ws) = p.workspace.as_deref() else {
            continue;
        };
        if Some(ws) == here.flatten() {
            continue;
        }
        match workspaces.iter_mut().find(|(name, _)| *name == ws) {
            Some((_, names)) => names.push(&p.name),
            None => workspaces.push((ws, vec![&p.name])),
        }
    }
    workspaces.sort_by(|a, b| a.0.cmp(b.0));
    if !workspaces.is_empty() {
        let body = workspaces
            .iter()
            .map(|(ws, names)| format!("{ws}: {}", names.join(", ")))
            .collect::<Vec<_>>()
            .join(" · ");
        lines.push(format!("Other workspaces: {body}"));
    }

    // Line 4: the projects in no workspace, names only — unless line 2
    // already listed them, which is exactly the thread being in one of
    // them.
    if here != Some(None) {
        let loose: Vec<&str> = rows
            .iter()
            .filter(|p| p.workspace.is_none())
            .map(|p| p.name.as_str())
            .collect();
        if !loose.is_empty() {
            lines.push(format!("In no workspace: {}", loose.join(", ")));
        }
    }

    Some(lines.join("\n"))
}

/// A root as it is shown: `~/…` when it lies strictly under `home`,
/// otherwise exactly as stored.
fn shown_root(root: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home
        && let Ok(rest) = root.strip_prefix(home)
        && !rest.as_os_str().is_empty()
    {
        return Path::new("~").join(rest).display().to_string();
    }
    root.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A project `name` with the given workspace: its root is
    /// `home/name`, or outside `home` when `outside`, and no brief.
    fn listed(home: &Path, name: &str, workspace: Option<&str>, outside: bool) -> Listed {
        let root = if outside {
            PathBuf::from("/elsewhere").join(name)
        } else {
            home.join(name)
        };
        Listed {
            name: name.to_owned(),
            root,
            workspace: workspace.map(str::to_owned),
            one_line: None,
        }
    }

    /// The same row, with a brief whose first line is `one_line` (issue
    /// #123).
    fn briefed(mut row: Listed, one_line: &str) -> Listed {
        row.one_line = Some(one_line.to_owned());
        row
    }

    /// What the listing shows for a root under `home`, from the fixture
    /// and the spec's rule: `~` and the rest.
    fn under_home(name: &str) -> String {
        format!("~/{name}")
    }

    /// Three workspaces with two projects each, plus one project in no
    /// workspace. The order is deliberately not name order: the listing
    /// sorts.
    fn fixture(home: &Path) -> Vec<Listed> {
        vec![
            listed(home, "gamma", Some("two"), false),
            listed(home, "alpha", Some("one"), false),
            listed(home, "eta", None, false),
            listed(home, "delta", Some("two"), true),
            listed(home, "zeta", Some("three"), false),
            listed(home, "beta", Some("one"), false),
            listed(home, "epsilon", Some("three"), false),
        ]
    }

    /// T1 (issue #81): the whole block, over a fixture of three
    /// workspaces of two projects and one project in no workspace, with
    /// the current project in the first workspace. Every line is as
    /// specified: the current project and its workspace, its workspace's
    /// projects with roots and the current one marked, the other
    /// workspaces in name order, and the loose project last. Roots under
    /// `home` are `~/…`, one outside it is as stored.
    #[test]
    fn the_listing_names_the_current_project_its_workspace_and_the_rest() {
        let home = PathBuf::from("/home/steve");
        let all = fixture(&home);
        let current = all.iter().find(|p| p.name == "beta").cloned();
        let text = projects_listing(&all, current.as_ref(), Some(&home)).unwrap();
        let expected = format!(
            "Projects in reach. This thread is in beta (one).\n\
             one: alpha {} · beta (here) {}\n\
             Other workspaces: three: epsilon, zeta · two: delta, gamma\n\
             In no workspace: eta",
            under_home("alpha"),
            under_home("beta"),
        );
        assert_eq!(text, expected);
        // The root outside `home` is as stored, not shortened.
        let other = all.iter().find(|p| p.name == "delta").unwrap();
        assert_eq!(shown_root(&other.root, Some(&home)), "/elsewhere/delta");
    }

    /// T2 (issue #81): a current project in no workspace gets its
    /// projects on line 2 and no line 4; no current project gets no line
    /// 2, every workspace on line 3, line 4 present and nothing marked;
    /// an empty `all` is no block at all; and a current project that is
    /// not in `all` is still named on line 1, with nothing marked.
    #[test]
    fn the_listing_holds_for_a_loose_current_no_current_and_no_projects() {
        let home = PathBuf::from("/home/steve");
        let all = fixture(&home);

        let loose = listed(&home, "eta", None, false);
        let text = projects_listing(&all, Some(&loose), Some(&home)).unwrap();
        let expected = format!(
            "Projects in reach. This thread is in eta (no workspace).\n\
             In no workspace: eta (here) {}\n\
             Other workspaces: one: alpha, beta · three: epsilon, zeta · two: delta, gamma",
            under_home("eta"),
        );
        assert_eq!(text, expected);

        // No current project: no line 2, and line 4 carries the loose
        // project with nothing marked.
        let text = projects_listing(&all, None, Some(&home)).unwrap();
        let expected = "Projects in reach. This thread is in no project.\n\
             Other workspaces: one: alpha, beta · three: epsilon, zeta · two: delta, gamma\n\
             In no workspace: eta";
        assert_eq!(text, expected);

        assert_eq!(projects_listing(&[], None, Some(&home)), None);
        assert_eq!(projects_listing(&[], Some(&loose), Some(&home)), None);

        // The current project is named from itself, and no row of `all`
        // is marked as it.
        let elsewhere = listed(&home, "omega", Some("one"), false);
        let text = projects_listing(&all, Some(&elsewhere), Some(&home)).unwrap();
        let expected = format!(
            "Projects in reach. This thread is in omega (one).\n\
             one: alpha {} · beta {}\n\
             Other workspaces: three: epsilon, zeta · two: delta, gamma\n\
             In no workspace: eta",
            under_home("alpha"),
            under_home("beta"),
        );
        assert_eq!(text, expected);
        assert!(!text.contains("(here)"), "{text}");
    }

    /// Without a `home` nothing is shortened, and a root equal to `home`
    /// is not `~` either: it is not strictly under it.
    #[test]
    fn roots_are_shortened_only_strictly_under_home() {
        let home = PathBuf::from("/home/steve");
        let all = vec![
            listed(&home, "alpha", Some("one"), false),
            Listed {
                name: "beta".into(),
                root: home.clone(),
                workspace: Some("one".into()),
                one_line: None,
            },
        ];
        let current = all.iter().find(|p| p.name == "alpha").cloned();
        let text = projects_listing(&all, current.as_ref(), Some(&home)).unwrap();
        assert!(
            text.contains(&format!("alpha (here) {}", under_home("alpha"))),
            "{text}"
        );
        assert!(text.contains("beta /home/steve"), "{text}");
        let text = projects_listing(&all, current.as_ref(), None).unwrap();
        assert!(text.contains("alpha (here) /home/steve/alpha"), "{text}");
    }

    /// T3 (issue #123): a row of this workspace carries its brief's
    /// one-liner after its root — the current project's row too — while
    /// another workspace's row stays names only, and a project with no
    /// brief reads as it did before briefs existed. Line 1's own project
    /// is the brief-less `beta`, so nothing outside line 2 moves.
    #[test]
    fn the_one_line_goes_on_this_workspaces_rows_after_the_root() {
        let home = PathBuf::from("/home/steve");
        let all: Vec<Listed> = fixture(&home)
            .into_iter()
            .map(|row| match row.name.as_str() {
                "alpha" => briefed(row, "the marketing site, Next.js on Vercel"),
                // Another workspace, and the loose project: their
                // one-liners are read, but the block renders names only.
                "gamma" => briefed(row, "the billing service"),
                "eta" => briefed(row, "the scratch project"),
                _ => row,
            })
            .collect();
        let current = all.iter().find(|p| p.name == "beta").cloned();
        let text = projects_listing(&all, current.as_ref(), Some(&home)).unwrap();
        let expected = format!(
            "Projects in reach. This thread is in beta (one).\n\
             one: alpha {} — the marketing site, Next.js on Vercel · beta (here) {}\n\
             Other workspaces: three: epsilon, zeta · two: delta, gamma\n\
             In no workspace: eta",
            under_home("alpha"),
            under_home("beta"),
        );
        assert_eq!(text, expected);

        // No brief anywhere: exactly the text of the fixture without
        // briefs, byte for byte (the test above, re-run here as the
        // baseline).
        let plain = fixture(&home);
        let text = projects_listing(&plain, current.as_ref(), Some(&home)).unwrap();
        let expected = format!(
            "Projects in reach. This thread is in beta (one).\n\
             one: alpha {} · beta (here) {}\n\
             Other workspaces: three: epsilon, zeta · two: delta, gamma\n\
             In no workspace: eta",
            under_home("alpha"),
            under_home("beta"),
        );
        assert_eq!(text, expected);
    }
}
