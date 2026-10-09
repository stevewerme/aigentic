//! The file boundary in the loop (issue #124): a file tool reaching
//! outside the project asks a person whatever the mode, a step denies it,
//! and a session grant covers one canonical directory at a time. Every
//! path is a real one under a temp base with real symlinks; nothing here
//! touches a real config or threads directory.

mod common;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use aigentic_core::{Author, ContentBlock, EventKind, ProviderEvent, ToolCall, UserId};
use aigentic_log::{PermissionRequestedPayload, ThreadLog, ToolResultPayload};
use aigentic_policy::Policy;
use aigentic_runtime::{Answer, Approver, Mode, ProjectRow, Runtime};
use aigentic_skills::{LockEntry, Lockfile, Roots, SkillSet};
use aigentic_tools::{DEFAULT_TIMEOUT, ToolRegistry, Workdir};
use common::{done, scripted};
use serde_json::json;

fn steve() -> Author {
    Author::User(UserId("steve".into()))
}

fn text(t: &str) -> ProviderEvent {
    ProviderEvent::TextDelta(t.into())
}

fn read_file(id: &str, path: impl Into<String>) -> ProviderEvent {
    ProviderEvent::ToolCall(ToolCall {
        id: id.into(),
        name: "read_file".into(),
        args: json!({"path": path.into()}),
    })
}

fn write_file(id: &str, path: impl Into<String>, content: &str) -> ProviderEvent {
    ProviderEvent::ToolCall(ToolCall {
        id: id.into(),
        name: "write_file".into(),
        args: json!({"path": path.into(), "content": content}),
    })
}

fn bash(id: &str, command: impl Into<String>) -> ProviderEvent {
    ProviderEvent::ToolCall(ToolCall {
        id: id.into(),
        name: "bash".into(),
        args: json!({"command": command.into()}),
    })
}

/// Answers from a script and records every ask.
struct ScriptedApprover {
    answers: VecDeque<Answer>,
    asked: Arc<Mutex<Vec<PermissionRequestedPayload>>>,
}

impl Approver for ScriptedApprover {
    fn author(&self) -> Author {
        steve()
    }
    fn ask(&mut self, request: &PermissionRequestedPayload) -> Answer {
        self.asked.lock().unwrap().push(request.clone());
        self.answers.pop_front().expect("approver script exhausted")
    }
    fn ask_human(&mut self, _question: &str) -> Option<String> {
        None
    }
}

/// A temp base: `project/`, `sibling/`, two outside directories `a/` and
/// `b/`, and a symlink `project/out` that is really `sibling/`.
struct Fixture {
    _dir: tempfile::TempDir,
    base: PathBuf,
    project: PathBuf,
    sibling: PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let project = base.join("project");
    let sibling = base.join("sibling");
    for name in ["project", "sibling", "a", "b"] {
        std::fs::create_dir_all(base.join(name)).unwrap();
    }
    std::fs::write(project.join("a.txt"), "in\n").unwrap();
    std::fs::create_dir_all(project.join(".aigentic")).unwrap();
    std::fs::write(sibling.join("x.txt"), "out\n").unwrap();
    std::fs::write(base.join("a/x.txt"), "in a\n").unwrap();
    std::fs::write(base.join("a/y.txt"), "in a too\n").unwrap();
    std::fs::write(base.join("b/z.txt"), "in b\n").unwrap();
    std::os::unix::fs::symlink(&sibling, project.join("out")).unwrap();
    Fixture {
        _dir: dir,
        base,
        project,
        sibling,
    }
}

struct Rig {
    f: Fixture,
    runtime: Runtime,
    asked: Arc<Mutex<Vec<PermissionRequestedPayload>>>,
}

impl Rig {
    /// `policy` is handed the project root, so the boundary is built over
    /// the real temp paths rather than a literal.
    fn new(
        f: Fixture,
        script: Vec<Vec<ProviderEvent>>,
        answers: Vec<Answer>,
        policy: impl FnOnce(&Path) -> Policy,
    ) -> Self {
        let log = ThreadLog::open(&f.base, ulid::Ulid::generate()).unwrap();
        let (provider, _seen) = scripted(script);
        let registry = ToolRegistry::builtin(Workdir::new(&f.project), DEFAULT_TIMEOUT);
        let asked = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            provider,
            registry,
            log,
            aigentic_core::AgentId("worker".into()),
        )
        .with_policy(policy(&f.project))
        .with_approver(Box::new(ScriptedApprover {
            answers: answers.into(),
            asked: asked.clone(),
        }));
        Rig { f, runtime, asked }
    }

    async fn go(&mut self) {
        self.runtime
            .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
            .await
            .unwrap();
    }

    /// The result of one call, found by its id: a human-decided call
    /// writes permission events before it, so a fixed index would not do.
    fn result(&self, call_id: &str) -> ToolResultPayload {
        for e in self.runtime.log().read_all().unwrap() {
            if e.kind != EventKind::ToolResult {
                continue;
            }
            let p: ToolResultPayload = serde_json::from_value(e.payload.clone()).unwrap();
            if p.result.id == call_id {
                return p;
            }
        }
        panic!("no tool result for {call_id}");
    }

    fn asked(&self) -> Vec<PermissionRequestedPayload> {
        self.asked.lock().unwrap().clone()
    }
}

/// The project root as the boundary gets it.
fn rooted(root: &Path) -> Policy {
    Policy::defaults().with_root(root, root)
}

/// T2: after a scripted `cd` out of the project, a relative path resolves
/// outside and asks; after a `cd` back inside it does not.
#[tokio::test]
async fn t2_a_cd_out_of_the_project_makes_a_relative_path_ask() {
    let f = fixture();
    let sibling = f.sibling.clone();
    let project = f.project.clone();
    let mut r = Rig::new(
        f,
        vec![
            vec![
                bash("c1", format!("cd {}", sibling.display())),
                read_file("c2", "x.txt"),
                bash("c3", format!("cd {}", project.display())),
                read_file("c4", "a.txt"),
                done("tool_use"),
            ],
            vec![text("done"), done("stop")],
        ],
        vec![Answer::Allow],
        rooted,
    );
    r.runtime.set_mode(Mode::Auto);
    r.go().await;
    let asked = r.asked();
    assert_eq!(asked.len(), 1, "only the outside read asked: {asked:?}");
    assert_eq!(asked[0].call.name, "read_file");
    assert_eq!(asked[0].call.args["path"], "x.txt");
    let expected = format!(
        "outside this project: {}",
        r.f.sibling.join("x.txt").display()
    );
    assert!(
        asked[0].reason.starts_with(&expected),
        "{}",
        asked[0].reason
    );
    // The outside read ran on the human's allow; the inside one did not ask.
    assert_eq!(r.result("c2").result.content, "out\n");
    assert!(
        !r.result("c4").result.is_error,
        "{}",
        r.result("c4").result.content
    );
    assert_eq!(r.result("c4").result.content, "in\n");
}

/// T3: modes never wave a boundary ask through. `accept-edits` still runs
/// an inside write unasked, asks before an outside one, and `auto` asks
/// before an outside read.
#[tokio::test]
async fn t3_a_mode_never_runs_an_outside_path() {
    let f = fixture();
    let outside_file = f.sibling.join("new.txt");
    let inside_file = f.project.join("new.txt");
    let mut r = Rig::new(
        f,
        vec![
            vec![
                write_file("c1", inside_file.display().to_string(), "new\n"),
                write_file("c2", outside_file.display().to_string(), "escaped\n"),
                done("tool_use"),
            ],
            vec![text("done"), done("stop")],
        ],
        vec![Answer::Deny],
        rooted,
    );
    r.runtime.set_mode(Mode::AcceptEdits);
    r.go().await;
    let asked = r.asked();
    assert_eq!(asked.len(), 1, "the outside write asked: {asked:?}");
    assert_eq!(asked[0].call.id, "c2");
    assert_eq!(
        asked[0].reason,
        format!("outside this project: {}", outside_file.display())
    );
    // The inside write ran under the mode, the outside one did not happen.
    assert!(
        !r.result("c1").result.is_error,
        "{}",
        r.result("c1").result.content
    );
    assert_eq!(std::fs::read_to_string(&inside_file).unwrap(), "new\n");
    assert!(r.result("c2").result.is_error);
    assert!(!outside_file.exists(), "the denied write wrote nothing");

    // `auto` asks too, and a granted read of an outside path is the
    // person's, not the mode's.
    let f = fixture();
    let outside_file = f.sibling.join("x.txt");
    let mut r = Rig::new(
        f,
        vec![
            vec![
                read_file("c1", outside_file.display().to_string()),
                done("tool_use"),
            ],
            vec![text("done"), done("stop")],
        ],
        vec![Answer::Allow],
        rooted,
    );
    r.runtime.set_mode(Mode::Auto);
    r.go().await;
    let asked = r.asked();
    assert_eq!(asked.len(), 1, "auto asked: {asked:?}");
    assert_eq!(
        asked[0].reason,
        format!("outside this project: {}", outside_file.display())
    );
    assert_eq!(r.result("c1").result.content, "out\n");
}

/// T7: a skill's own folder is a boundary root, and `load_skill` says
/// where it is, in canonical absolute form.
#[tokio::test]
async fn t7_load_skill_names_its_folder_and_that_folder_is_reachable() {
    let f = fixture();
    // A skill folder beside the project, outside it, with a file the
    // skill's body cites.
    let skill_root = f.base.join("skills");
    let skill_dir = skill_root.join("tdd");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: tdd\ndescription: The tdd skill.\n---\n\n# TDD\n\nSee ./NOTES.md.\n",
    )
    .unwrap();
    std::fs::write(skill_dir.join("NOTES.md"), "the notes\n").unwrap();
    let roots = Roots {
        project: None,
        user: None,
        bundled: Some(skill_root),
    };
    let mut lock = Lockfile::default();
    for m in aigentic_skills::discover_roots(&roots).unwrap() {
        lock.upsert(
            LockEntry::from_manifest(&m, format!("skills/{}", m.name), "local", "abc").unwrap(),
        );
    }
    let skills = SkillSet::load(&["tdd".into()], &roots, &lock, &[]).unwrap();
    let canonical = std::fs::canonicalize(&skill_dir).unwrap();
    let notes = canonical.join("NOTES.md");
    let boundary_root = canonical.clone();

    let mut r = Rig::new(
        f,
        vec![
            vec![
                ProviderEvent::ToolCall(ToolCall {
                    id: "c1".into(),
                    name: "load_skill".into(),
                    args: json!({"name": "tdd"}),
                }),
                read_file("c2", notes.display().to_string()),
                done("tool_use"),
            ],
            vec![text("done"), done("stop")],
        ],
        vec![],
        move |root| {
            Policy::defaults()
                .with_root(root, root)
                .with_boundary_roots([boundary_root])
        },
    );
    r.runtime = r.runtime.with_skills(skills);
    r.go().await;
    let loaded = r.result("c1");
    assert!(!loaded.result.is_error, "{}", loaded.result.content);
    assert_eq!(
        loaded.result.content,
        format!(
            "loaded skill `tdd`; its instructions are now in context\nfiles beside this skill are in {}",
            canonical.display()
        )
    );
    assert!(
        r.asked().is_empty(),
        "the skill folder is inside: {:?}",
        r.asked()
    );
    let read = r.result("c2");
    assert!(!read.result.is_error, "{}", read.result.content);
    assert_eq!(read.result.content, "the notes\n");
}

/// T4: "allow for this session" outside covers one canonical directory,
/// and a tool-wide grant from an inside call does not answer an outside
/// one. The outside paths are inside the temp base only because the
/// fixture is; the boundary is the project.
#[tokio::test]
async fn t4_a_grant_covers_one_directory_and_never_a_tool() {
    let f = fixture();
    let a = f.base.join("a");
    let b = f.base.join("b");
    let mut r = Rig::new(
        f,
        vec![
            vec![
                read_file("c1", a.join("x.txt").display().to_string()),
                read_file("c2", a.join("y.txt").display().to_string()),
                read_file("c3", b.join("z.txt").display().to_string()),
                done("tool_use"),
            ],
            vec![text("done"), done("stop")],
        ],
        vec![Answer::AllowForSession, Answer::Deny],
        rooted,
    );
    r.go().await;
    let asked = r.asked();
    assert_eq!(asked.len(), 2, "the grant did not answer `b`: {asked:?}");
    assert_eq!(asked[0].call.id, "c1");
    assert_eq!(asked[1].call.id, "c3");
    assert_eq!(r.result("c1").result.content, "in a\n");
    assert_eq!(r.result("c2").result.content, "in a too\n");
    assert!(r.result("c3").result.is_error, "b was declined");

    // The inside grant is tool-wide, and must still not answer an outside
    // call: only a boundary path scope does.
    let f = fixture();
    let project = f.project.clone();
    let sibling = f.sibling.clone();
    let mut r = Rig::new(
        f,
        vec![
            vec![
                write_file("c1", "in.txt", "hi\n"),
                read_file("c2", sibling.join("x.txt").display().to_string()),
                done("tool_use"),
            ],
            vec![text("done"), done("stop")],
        ],
        vec![Answer::AllowForSession, Answer::Deny],
        rooted,
    );
    r.go().await;
    let asked = r.asked();
    assert_eq!(
        asked.len(),
        2,
        "the tool-wide grant did not cover: {asked:?}"
    );
    assert_eq!(asked[0].call.id, "c1");
    assert_eq!(asked[1].call.id, "c2");
    assert!(
        !r.result("c1").result.is_error,
        "{}",
        r.result("c1").result.content
    );
    assert_eq!(
        std::fs::read_to_string(project.join("in.txt")).unwrap(),
        "hi\n"
    );
    assert!(
        r.result("c2").result.is_error,
        "the outside read was denied"
    );
}

/// T3, step half: in a step thread a boundary ask is a deny, with no
/// person asked, and an inside path still runs.
#[tokio::test]
async fn t3_a_step_denies_an_outside_path_and_runs_an_inside_one() {
    let f = fixture();
    let outside_file = f.sibling.join("x.txt");
    let mut r = Rig::new(
        f,
        vec![
            vec![
                read_file("c1", outside_file.display().to_string()),
                read_file("c2", "a.txt"),
                done("tool_use"),
            ],
            vec![text("done"), done("stop")],
        ],
        vec![],
        rooted,
    );
    r.runtime = r
        .runtime
        .with_step("implement", &[])
        .expect("a known deny list");
    r.go().await;
    assert!(r.asked().is_empty(), "a step asks nobody");
    let denied = r.result("c1");
    assert!(denied.result.is_error);
    assert_eq!(
        denied.result.content,
        format!(
            "denied by policy: boundary (outside this project: {} — a step never reaches another project)",
            outside_file.display()
        ),
        "the refusal names the path"
    );
    let inside = r.result("c2");
    assert!(!inside.result.is_error, "{}", inside.result.content);
    assert_eq!(inside.result.content, "in\n");
}

/// T6: `read_brief` still reads a sibling's brief with the boundary on —
/// it takes no path — while a `read_file` on that same brief asks.
#[tokio::test]
async fn t6_read_brief_still_works_and_read_file_on_it_asks() {
    let f = fixture();
    // A sibling project `q` with a brief, and rows as the daemon renders
    // them.
    let q = f.base.join("q");
    std::fs::create_dir_all(q.join(".aigentic")).unwrap();
    std::fs::write(q.join("aigentic.toml"), "[project]\nname = \"q\"\n").unwrap();
    let brief = q.join(".aigentic/brief.md");
    std::fs::write(&brief, "# Q\n\nThe marketing site.\n").unwrap();
    let rows = vec![
        ProjectRow {
            name: "q".into(),
            root: q.clone(),
            workspace: None,
            one_line: Some("The marketing site.".into()),
            understood: true,
        },
        ProjectRow {
            name: "p".into(),
            root: f.project.clone(),
            workspace: None,
            one_line: None,
            understood: true,
        },
    ];
    let mut r = Rig::new(
        f,
        vec![
            vec![
                ProviderEvent::ToolCall(ToolCall {
                    id: "c1".into(),
                    name: "read_brief".into(),
                    args: json!({"project": "q"}),
                }),
                read_file("c2", brief.display().to_string()),
                done("tool_use"),
            ],
            vec![text("done"), done("stop")],
        ],
        vec![Answer::Deny],
        rooted,
    );
    r.runtime = r
        .runtime
        .with_projects(Some("q — The marketing site.".into()), rows);
    r.go().await;
    let read_brief = r.result("c1");
    assert!(!read_brief.result.is_error, "{}", read_brief.result.content);
    assert!(
        read_brief.result.content.contains("The marketing site."),
        "{}",
        read_brief.result.content
    );
    let asked = r.asked();
    assert_eq!(asked.len(), 1, "only the file tool asked: {asked:?}");
    assert_eq!(asked[0].call.name, "read_file");
    assert_eq!(
        asked[0].reason,
        format!("outside this project: {}", brief.display())
    );
    assert!(r.result("c2").result.is_error);
}

/// A `cd` into `.aigentic` must not let a relative write past the memory
/// deny: the rules resolve from the live directory (the second half of
/// T8, end to end).
#[tokio::test]
async fn t8_a_cd_into_the_agents_folder_does_not_walk_past_the_memory_deny() {
    let f = fixture();
    let agents = f.project.join(".aigentic");
    let mut r = Rig::new(
        f,
        vec![
            vec![
                bash("c1", format!("cd {}", agents.display())),
                write_file("c2", "memory/facts.md", "a fact\n"),
                done("tool_use"),
            ],
            vec![text("done"), done("stop")],
        ],
        vec![],
        rooted,
    );
    r.runtime.set_mode(Mode::Auto);
    r.go().await;
    assert!(
        r.asked().is_empty(),
        "the deny needs no person: {:?}",
        r.asked()
    );
    let denied = r.result("c2").result;
    assert!(denied.is_error);
    assert!(
        denied.content.contains("denied by policy"),
        "{}",
        denied.content
    );
    assert!(!agents.join("memory/facts.md").exists());
}

/// The five file tools are the boundary's business and nothing else's: a
/// tool outside them keeps the rules' answer.
#[tokio::test]
async fn only_the_file_tools_are_confined() {
    // `bash` reading an outside path is unconfined by design until the
    // sandbox: the call runs, and no ask names a boundary.
    let f = fixture();
    let outside = f.sibling.join("x.txt");
    let mut r = Rig::new(
        f,
        vec![
            vec![
                bash("c1", format!("cat {}", outside.display())),
                done("tool_use"),
            ],
            vec![text("done"), done("stop")],
        ],
        vec![Answer::Allow],
        rooted,
    );
    r.runtime.set_mode(Mode::Auto);
    r.go().await;
    let ask = r.asked().into_iter().find(|a| a.call.name == "bash");
    assert!(ask.is_none(), "auto ran the shell");
    let result = r.result("c1").result;
    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("out"), "{}", result.content);
    assert!(
        !r.asked()
            .iter()
            .any(|a| a.reason.contains("outside this project")),
        "no boundary ask for a shell"
    );
}

/// A dangling symlink inside the project is not an inside path: the link
/// resolves, by hand, to its target outside, so a write through it asks a
/// person in `accept-edits` and `auto` even though the link's own name is
/// inside, and a denial leaves the target uncreated.
#[tokio::test]
async fn a_dangling_link_inside_does_not_carry_a_write_outside() {
    for mode in [Mode::AcceptEdits, Mode::Auto] {
        let f = fixture();
        let ghost = f.sibling.join("ghost.txt");
        std::os::unix::fs::symlink(&ghost, f.project.join("link-dangling")).unwrap();
        let mut r = Rig::new(
            f,
            vec![
                vec![
                    write_file("c1", "link-dangling", "escaped\n"),
                    done("tool_use"),
                ],
                vec![text("done"), done("stop")],
            ],
            vec![Answer::Deny],
            rooted,
        );
        r.runtime.set_mode(mode);
        r.go().await;
        let asked = r.asked();
        assert_eq!(asked.len(), 1, "{mode:?} asked: {asked:?}");
        assert_eq!(asked[0].call.id, "c1");
        assert_eq!(
            asked[0].reason,
            format!("outside this project: {}", ghost.display())
        );
        assert!(r.result("c1").result.is_error, "{mode:?}");
        assert!(
            !ghost.exists(),
            "{mode:?}: the denied write created {} through the link",
            ghost.display()
        );
    }
}

/// The same link under a step is denied with nobody to ask, and the
/// target stays absent.
#[tokio::test]
async fn a_step_denies_a_write_through_a_dangling_link() {
    let f = fixture();
    let ghost = f.sibling.join("ghost.txt");
    std::os::unix::fs::symlink(&ghost, f.project.join("link-dangling")).unwrap();
    let mut r = Rig::new(
        f,
        vec![
            vec![
                write_file("c1", "link-dangling", "escaped\n"),
                done("tool_use"),
            ],
            vec![text("done"), done("stop")],
        ],
        vec![],
        rooted,
    );
    r.runtime = r
        .runtime
        .with_step("implement", &[])
        .expect("a known deny list");
    r.go().await;
    assert!(r.asked().is_empty(), "a step asks nobody");
    let denied = r.result("c1");
    assert!(denied.result.is_error);
    assert_eq!(
        denied.result.content,
        format!(
            "denied by policy: boundary (outside this project: {} — a step never reaches another project)",
            ghost.display()
        ),
        "the refusal names the resolved target"
    );
    assert!(!ghost.exists(), "the denied write created nothing");
}
