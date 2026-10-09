//! The file boundary, pure (issue #124): the five file tools reach the
//! project, the temp dir and the folders the policy was given, and nowhere
//! else. Outside they ask — with the canonical path in the reason — before
//! any rule is read; with no root, nothing is inside and every checked call
//! asks. Every path here is a real one under a temp base, with real
//! symlinks, and no test touches a real config or thread directory.

use std::fs;
use std::path::PathBuf;

use aigentic_core::{RiskClass, ToolCall};
use aigentic_policy::{Decision, Outcome, Policy, Rule, canonicalise};
use serde_json::json;

/// The five checked tools, with the class the runtime gives each.
const TOOLS: [(&str, RiskClass); 5] = [
    ("read_file", RiskClass::Read),
    ("write_file", RiskClass::Write),
    ("edit_file", RiskClass::Write),
    ("list_dir", RiskClass::Read),
    ("grep", RiskClass::Read),
];

fn call(tool: &str, path: &str) -> ToolCall {
    ToolCall {
        id: "c".into(),
        name: tool.into(),
        args: json!({"path": path}),
    }
}

/// The same call with no `path` at all, as `list_dir` and `grep` are made.
fn call_without_path(tool: &str) -> ToolCall {
    ToolCall {
        id: "c".into(),
        name: tool.into(),
        args: json!({}),
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    /// The canonical temp base: what every expectation is built from.
    base: PathBuf,
    project: PathBuf,
    sibling: PathBuf,
    skill: PathBuf,
    opened: PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    // The base's own canonical form: on macOS the temp dir leads through
    // `/var`, a symlink, and the policy canonicalises its roots.
    let base = fs::canonicalize(dir.path()).unwrap();
    let project = base.join("project");
    let sibling = base.join("sibling");
    let skill = base.join("skill");
    let opened = base.join("opened");
    for d in [&project, &sibling, &skill, &opened] {
        fs::create_dir_all(d).unwrap();
    }
    // A directory inside the project that is really the sibling.
    std::os::unix::fs::symlink(&sibling, project.join("out")).unwrap();
    fs::write(project.join("a.txt"), "in\n").unwrap();
    fs::write(sibling.join("x.txt"), "out\n").unwrap();
    fs::write(skill.join("CONTEXT-FORMAT.md"), "skill\n").unwrap();
    fs::write(opened.join("notes.md"), "opened\n").unwrap();
    Fixture {
        _dir: dir,
        base,
        project,
        sibling,
        skill,
        opened,
    }
}

/// Every class allows, so an inside call's answer is the fixture's rule and
/// an outside call's is the boundary's, with nothing else in play.
fn rules() -> Vec<Rule> {
    [
        RiskClass::Read,
        RiskClass::Write,
        RiskClass::Exec,
        RiskClass::Network,
        RiskClass::Safe,
    ]
    .into_iter()
    .map(|class| Rule::class(class, Decision::Allow, "fixture allow"))
    .collect()
}

impl Fixture {
    /// The policy under test: the project root, a skill folder and an
    /// `allow_paths` entry. The system temp dir is not a root here — the
    /// fixture base lives inside it, so it would swallow every escape this
    /// file is about; the temp root has its own test below.
    fn policy(&self) -> Policy {
        Policy::configured(rules(), None)
            .with_root(&self.project, &self.project)
            .with_boundary_roots([self.skill.clone(), self.opened.clone()])
    }

    /// The defaults — the memory deny among them — over the project root.
    fn defaults(&self) -> Policy {
        Policy::defaults().with_root(&self.project, &self.project)
    }

    /// The same rules with the whole temp base as the boundary, so every
    /// path in the fixture is inside: the answer this gives is the rules'
    /// decision, which nothing in the boundary should change.
    fn rules_only(&self) -> Policy {
        Policy::configured(rules(), None)
            .with_root(&self.project, &self.project)
            .with_boundary_roots([self.base.clone()])
    }
}

/// The canonical path a boundary ask names.
fn asked(outcome: &Outcome) -> PathBuf {
    match outcome {
        Outcome::AskBoundary { reason, path } => {
            let path = path.clone().expect("a canonical path");
            let expected = format!("outside this project: {}", path.display());
            assert_eq!(*reason, expected, "the reason carries the canonical path");
            path
        }
        other => panic!("expected a boundary ask, got {other:?}"),
    }
}

/// Inside the project the rules decide, for every checked tool.
#[test]
fn t1_inside_the_project_is_the_rules_decider() {
    let f = fixture();
    let p = f.policy();
    for (tool, class) in TOOLS {
        for path in [
            f.project.join("a.txt").display().to_string(),
            "a.txt".to_owned(),
            "sub/../a.txt".to_owned(),
        ] {
            let call = call(tool, &path);
            let expected = f.rules_only().decide(&call, class, Some(&f.project));
            assert!(
                matches!(expected, Outcome::Allow { .. }),
                "the fixture's rules allow: {expected:?}"
            );
            assert_eq!(
                p.decide(&call, class, Some(&f.project)),
                expected,
                "{tool} {path}"
            );
        }
    }
}

/// The temp dir, a skill folder and an `allow_paths` entry are roots.
#[test]
fn t1_the_temp_dir_a_skill_folder_and_allow_paths_are_inside() {
    let f = fixture();
    let p = f.policy();
    for (tool, class) in TOOLS {
        for path in [f.skill.join("CONTEXT-FORMAT.md"), f.opened.join("notes.md")] {
            let call = call(tool, &path.display().to_string());
            let expected = f.rules_only().decide(&call, class, Some(&f.project));
            assert!(
                matches!(expected, Outcome::Allow { .. }),
                "the fixture's rules allow: {expected:?}"
            );
            assert_eq!(
                p.decide(&call, class, Some(&f.project)),
                expected,
                "{tool} {path:?} is a boundary root"
            );
        }
    }
    // The temp dir stands on its own, since the fixture lives inside it.
    let temp = std::env::temp_dir();
    let with_temp = Policy::configured(rules(), None)
        .with_root(&f.project, &f.project)
        .with_boundary_roots([temp.clone()]);
    for (tool, class) in TOOLS {
        let inside = call(
            tool,
            &temp.join("124-not-created.txt").display().to_string(),
        );
        let decided = with_temp.decide(&inside, class, Some(&f.project));
        // The fixture's rules allow every class, so anything but an allow
        // here means the boundary refused a temp path.
        assert!(
            matches!(decided, Outcome::Allow { .. }),
            "{tool} under the temp dir is inside: {decided:?}"
        );
        // And the temp root did not swallow everything: a path outside
        // every root still asks.
        let outside = call(tool, "/the-absolute-sibling/x.txt");
        assert!(
            matches!(
                with_temp.decide(&outside, class, Some(&f.project)),
                Outcome::AskBoundary { .. }
            ),
            "{tool} outside every root"
        );
    }
}

/// Each escape the spec names, for every checked tool: an absolute path
/// outside, a `../sibling/x`, a symlink inside pointing out, and a new file
/// under an outside or symlinked directory. Each asks, and names the
/// canonical path — the symlink's target, not the link.
#[test]
fn t1_each_way_out_of_the_project_asks_with_the_canonical_path() {
    let f = fixture();
    let p = f.policy();
    let absolute = PathBuf::from("/the-absolute-sibling/x.txt");
    let outside_file = f.sibling.join("x.txt");
    let fresh_outside = f.sibling.join("not-created-yet.txt");
    for (tool, class) in TOOLS {
        let cases: [(&str, &PathBuf); 5] = [
            // An absolute path outside every root.
            (absolute.to_str().unwrap(), &absolute),
            // A parent hop from the project.
            ("../sibling/x.txt", &outside_file),
            // A symlink inside the project that is really the sibling.
            ("out/x.txt", &outside_file),
            // A file that does not exist yet, outside the project: the
            // nearest real ancestor plus the rest.
            (fresh_outside.to_str().unwrap(), &fresh_outside),
            // The same through the symlink, for a write.
            ("out/not-created-yet.txt", &fresh_outside),
        ];
        for (raw, expected) in cases {
            let call = call(tool, raw);
            assert_eq!(
                asked(&p.decide(&call, class, Some(&f.project))),
                fs::canonicalize(expected).unwrap_or_else(|_| expected.clone()),
                "{tool} {raw}"
            );
        }
        // A missing `path` on `list_dir` or `grep` is the working
        // directory, so an outside one asks too.
        if matches!(tool, "list_dir" | "grep") {
            let call = call_without_path(tool);
            assert_eq!(
                asked(&p.decide(&call, class, Some(&f.sibling))),
                fs::canonicalize(&f.sibling).unwrap(),
                "{tool} with no path from an outside directory"
            );
            let expected = f.rules_only().decide(&call, class, Some(&f.project));
            assert_eq!(
                p.decide(&call, class, Some(&f.project)),
                expected,
                "{tool} with no path from the project"
            );
        }
    }
}

/// `..` climbs out of the project even when it starts inside it.
#[test]
fn t1_a_parent_hop_out_of_the_project_asks() {
    let f = fixture();
    let p = f.policy();
    let (tool, class) = ("read_file", RiskClass::Read);
    assert!(matches!(
        p.decide(&call(tool, "../sibling/./x.txt"), class, Some(&f.project)),
        Outcome::AskBoundary { .. }
    ));
    assert!(matches!(
        p.decide(&call(tool, "../project/../sibling/x.txt"), class, Some(&f.project)),
        Outcome::AskBoundary {
            path,
            ..
        } if path.as_deref() == Some(f.sibling.join("x.txt").as_path())
    ));
}

/// No root: nothing is inside, so every checked call asks — including a
/// relative one, which the rules would otherwise have read as given.
#[test]
fn t1_with_no_root_every_checked_call_asks() {
    let f = fixture();
    let bare = Policy::defaults();
    for (tool, class) in TOOLS {
        for raw in ["a.txt", ".aigentic/memory/facts.md"] {
            let call = call(tool, raw);
            assert!(
                matches!(
                    bare.decide(&call, class, Some(&f.project)),
                    Outcome::AskBoundary { .. }
                ),
                "{tool} {raw} with no root"
            );
        }
    }
}

/// A path rule resolves from the live directory, so a `cd` into
/// `.aigentic` cannot step around the memory deny (issue #124's escape).
#[test]
fn t8_the_memory_deny_after_a_cd_into_the_agents_folder() {
    let f = fixture();
    let p = f.defaults();
    let from_root = p.decide(
        &call("write_file", ".aigentic/memory/x.md"),
        RiskClass::Write,
        Some(&f.project),
    );
    assert!(
        matches!(from_root, Outcome::Deny { .. }),
        "the memory folder is denied: {from_root:?}"
    );
    let after_cd = p.decide(
        &call("write_file", "memory/x.md"),
        RiskClass::Write,
        Some(&f.project.join(".aigentic")),
    );
    assert_eq!(after_cd, from_root, "the live directory is the base");
}

/// The five checked names are the boundary's business and nothing else's:
/// `bash`, the harness tools and the MCP tools keep the rules' answer even
/// when their arguments name an outside path.
#[test]
fn t1_only_the_file_tools_are_checked() {
    let f = fixture();
    let p = f.policy();
    let outside = f.sibling.join("x.txt").display().to_string();
    for tool in ["bash", "mcp.docs.read", "read_brief", "load_skill"] {
        let call = call(tool, &outside);
        let expected = f
            .rules_only()
            .decide(&call, RiskClass::Safe, Some(&f.project));
        assert_eq!(
            p.decide(&call, RiskClass::Safe, Some(&f.project)),
            expected,
            "{tool} is not confined by the boundary"
        );
    }
}

/// A dangling symlink is judged by where it points, not by its path:
/// `canonicalize()` fails for it, and the write the link carries lands
/// at the target, so the link to an outside file is outside the project.
#[test]
fn a_dangling_symlink_to_an_outside_file_is_outside() {
    let f = fixture();
    let target = f.sibling.join("not-created.txt");
    std::os::unix::fs::symlink(&target, f.project.join("link-dangling")).unwrap();
    let p = f.policy();
    assert_eq!(
        canonicalise(&f.project.join("link-dangling")),
        target,
        "resolution follows the link even though the target is missing"
    );
    let call = call("write_file", "link-dangling");
    assert_eq!(
        asked(&p.decide(&call, RiskClass::Write, Some(&f.project))),
        target,
        "the ask names the file the write would create"
    );
}

/// The same link, pointing at a file that does not exist inside the
/// project: the write stays in, so the boundary has nothing to ask.
#[test]
fn a_dangling_symlink_to_an_inside_file_is_inside() {
    let f = fixture();
    let target = f.project.join("not-created.txt");
    std::os::unix::fs::symlink(&target, f.project.join("link-dangling")).unwrap();
    let p = f.policy();
    let call = call("write_file", "link-dangling");
    assert!(
        matches!(
            p.decide(&call, RiskClass::Write, Some(&f.project)),
            Outcome::Allow { .. }
        ),
        "the write lands inside the project"
    );
}

/// A chain of links longer than the kernel follows cannot be resolved,
/// so the call asks rather than being judged by the link path.
#[test]
fn a_symlink_chain_past_the_hop_bound_is_outside() {
    let f = fixture();
    let mut next = f.sibling.join("unreachable.txt");
    for i in (0..45).rev() {
        let link = f.project.join(format!("chain{i}"));
        std::os::unix::fs::symlink(&next, &link).unwrap();
        next = link;
    }
    let p = f.policy();
    let call = call("read_file", "chain0");
    assert!(
        matches!(
            p.decide(&call, RiskClass::Read, Some(&f.project)),
            Outcome::AskBoundary { .. }
        ),
        "45 hops are past the bound"
    );
}

/// The whole path is canonicalised in one call, so a deep inside path
/// costs a syscall per check and not one per component.
#[test]
fn a_deep_inside_path_costs_one_call_per_check() {
    let f = fixture();
    let p = f.policy();
    let mut dir = f.project.clone();
    for i in 0..60 {
        dir = dir.join(format!("d{i}"));
    }
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("a.txt");
    fs::write(&file, "in\n").unwrap();
    let call = call("read_file", &file.display().to_string());
    let start = std::time::Instant::now();
    for _ in 0..1000 {
        assert!(
            matches!(
                p.decide(&call, RiskClass::Read, Some(&f.project)),
                Outcome::Allow { .. }
            ),
            "inside, in one call"
        );
    }
    let took = start.elapsed();
    eprintln!("1000 checks on a 60-component inside path: {took:?}");
    assert!(
        took < std::time::Duration::from_secs(1),
        "1000 checks took {took:?}"
    );
}
