//! Skills in the loop: the prefix line, `load_skill`, the user-invoked
//! rule, and `invoke_skill`.

mod common;

use aigentic_core::{Author, ContentBlock, EventKind, ProviderEvent, Role, ToolCall, UserId};
use aigentic_log::{Invoker, SkillLoadedPayload, ThreadLog, ToolResultPayload};
use aigentic_runtime::{Runtime, RuntimeError};
use aigentic_skills::{LockEntry, Lockfile, Roots, SkillSet};
use aigentic_tools::ToolRegistry;
use common::{Seen, done, scripted};
use serde_json::json;

fn steve() -> Author {
    Author::User(UserId("steve".into()))
}

fn write_skill(root: &std::path::Path, name: &str, user_invoked: bool, body: &str) {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let flag = if user_invoked {
        "disable-model-invocation: true\n"
    } else {
        ""
    };
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: The {name} skill.\n{flag}---\n\n{body}\n"),
    )
    .unwrap();
}

/// Two skills, `tdd` (model) and `implement` (user), locked and loaded.
fn skill_set(dir: &std::path::Path) -> SkillSet {
    let root = dir.join("skills");
    write_skill(&root, "tdd", false, "# TDD\n\nRed, green, refactor.");
    write_skill(&root, "implement", true, "# Implement\n\nUse /tdd.");
    let roots = Roots {
        project: None,
        user: None,
        bundled: Some(root),
    };
    let mut lock = Lockfile::default();
    for m in aigentic_skills::discover_roots(&roots).unwrap() {
        lock.upsert(
            LockEntry::from_manifest(&m, format!("skills/{}", m.name), "local", "abc").unwrap(),
        );
    }
    SkillSet::load(&["tdd".into(), "implement".into()], &roots, &lock, &[]).unwrap()
}

fn rig(script: Vec<Vec<ProviderEvent>>) -> (Runtime, Seen, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let log = ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap();
    let (provider, seen) = scripted(script);
    let runtime = Runtime::new(
        provider,
        ToolRegistry::empty(),
        log,
        aigentic_core::AgentId("worker".into()),
    )
    .with_layers(aigentic_runtime::Layers::global_instructions("Be terse."))
    .with_skills(skill_set(dir.path()));
    (runtime, seen, dir)
}

fn load(id: &str, name: &str) -> ProviderEvent {
    ProviderEvent::ToolCall(ToolCall {
        id: id.into(),
        name: "load_skill".into(),
        args: json!({"name": name}),
    })
}

fn text(t: &str) -> ProviderEvent {
    ProviderEvent::TextDelta(t.into())
}

fn texts(m: &aigentic_core::Message) -> String {
    m.blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

#[tokio::test]
async fn the_prefix_lists_skills_after_pins_and_offers_load_skill() {
    let (mut rt, seen, _dir) = rig(vec![vec![text("ok"), done("stop")]]);
    rt.pin(steve(), "Use Swedish.".into(), &mut |_| {}).unwrap();
    let names: Vec<String> = rt.tool_specs().into_iter().map(|s| s.name).collect();
    assert_eq!(names, vec!["ask_human", "load_skill", "pin"]);

    rt.run_turn(steve(), vec![ContentBlock::Text("hi".into())], &mut |_| {})
        .await
        .unwrap();
    let seen = seen.lock().unwrap();
    let ctx = &seen[0];
    assert_eq!(ctx.len(), 4);
    assert_eq!(texts(&ctx[0]), "Be terse.");
    assert!(texts(&ctx[1]).starts_with("Pinned facts:"));
    assert_eq!(ctx[2].role, Role::System);
    assert_eq!(
        texts(&ctx[2]),
        "# Skills\n\n\
         Run by the user as slash commands; do not load them yourself:\n\
         - implement: The implement skill.\n\n\
         Available through the `load_skill` tool when the description fits the task. When a loaded skill's instructions refer to `/<name>` and that name is in this list, call `load_skill` with it before continuing:\n\
         - tdd: The tdd skill."
    );
    assert_eq!(texts(&ctx[3]), "hi");
}

#[tokio::test]
async fn without_skills_there_is_no_prefix_block_and_no_load_skill() {
    let dir = tempfile::tempdir().unwrap();
    let log = ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap();
    let (provider, seen) = scripted(vec![vec![text("ok"), done("stop")]]);
    let mut rt = Runtime::new(
        provider,
        ToolRegistry::empty(),
        log,
        aigentic_core::AgentId("worker".into()),
    );
    let names: Vec<String> = rt.tool_specs().into_iter().map(|s| s.name).collect();
    assert_eq!(names, vec!["ask_human", "pin"]);
    rt.run_turn(steve(), vec![ContentBlock::Text("hi".into())], &mut |_| {})
        .await
        .unwrap();
    assert_eq!(seen.lock().unwrap()[0].len(), 1);
}

#[tokio::test]
async fn load_skill_appends_skill_loaded_and_the_body_is_in_the_next_request() {
    let (mut rt, seen, _dir) = rig(vec![
        vec![load("c1", "tdd"), done("tool_use")],
        vec![text("loaded, starting"), done("stop")],
    ]);
    rt.run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
        .await
        .unwrap();
    let events = rt.log().read_all().unwrap();
    let kinds: Vec<EventKind> = events.iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        vec![
            EventKind::UserMessage,
            EventKind::AssistantMessage,
            EventKind::SkillLoaded,
            EventKind::ToolResult,
            EventKind::AssistantMessage,
            EventKind::TurnEnded,
        ]
    );
    assert_eq!(
        events[2].author,
        Author::Agent(aigentic_core::AgentId("worker".into()))
    );
    let p: SkillLoadedPayload = serde_json::from_value(events[2].payload.clone()).unwrap();
    assert_eq!(p.name, "tdd");
    assert_eq!(p.invoked_by, Invoker::Model);
    assert_eq!(p.source, "local@abc");
    assert_eq!(p.hash.len(), 64);
    assert_eq!(p.body, "# TDD\n\nRed, green, refactor.\n");
    let r: ToolResultPayload = serde_json::from_value(events[3].payload.clone()).unwrap();
    assert!(!r.result.is_error);
    assert!(r.result.content.contains("loaded skill `tdd`"));
    assert!(r.policy.is_some());

    // The second request carries the skill body as a system-authored user message.
    let seen = seen.lock().unwrap();
    let second = &seen[1];
    let skill_msg = second
        .iter()
        .find(|m| m.author == Author::System && m.role == Role::User)
        .expect("skill body in context");
    assert_eq!(
        texts(skill_msg),
        "[Skill `tdd` loaded; follow it for this task]\n\n# TDD\n\nRed, green, refactor.\n"
    );
    let idx = second
        .iter()
        .position(|m| std::ptr::eq(m, skill_msg))
        .unwrap();
    assert!(
        matches!(second[idx - 1].role, Role::Tool),
        "body follows the tool result, never splitting a call from its result"
    );
    assert!(matches!(second[idx - 2].role, Role::Assistant));
}

#[tokio::test]
async fn a_model_invoked_skill_cannot_load_a_user_invoked_one() {
    let (mut rt, _seen, _dir) = rig(vec![
        vec![
            load("c1", "implement"),
            load("c2", "nope"),
            done("tool_use"),
        ],
        vec![text("ok"), done("stop")],
    ]);
    rt.run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
        .await
        .unwrap();
    let events = rt.log().read_all().unwrap();
    assert!(
        events.iter().all(|e| e.kind != EventKind::SkillLoaded),
        "nothing loaded"
    );
    let r: ToolResultPayload = serde_json::from_value(events[2].payload.clone()).unwrap();
    assert!(r.result.is_error);
    assert!(
        r.result.content.contains("user-invoked"),
        "{}",
        r.result.content
    );
    assert!(r.result.content.contains("/implement"));
    let r: ToolResultPayload = serde_json::from_value(events[3].payload.clone()).unwrap();
    assert!(r.result.is_error);
    assert!(r.result.content.contains("unknown skill `nope`"));
}

#[tokio::test]
async fn invoke_skill_loads_then_runs_the_turn_with_the_arguments() {
    let (mut rt, seen, _dir) = rig(vec![vec![text("implementing"), done("stop")]]);
    let outcome = rt
        .invoke_skill(
            steve(),
            "implement",
            "fix the off-by-one in cost.rs",
            &mut |_| {},
        )
        .await
        .unwrap();
    assert_eq!(outcome.reason, "done");
    let events = rt.log().read_all().unwrap();
    let kinds: Vec<EventKind> = events.iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        vec![
            EventKind::SkillLoaded,
            EventKind::UserMessage,
            EventKind::AssistantMessage,
            EventKind::TurnEnded,
        ]
    );
    assert_eq!(
        events[0].author,
        steve(),
        "attributed to who ran the slash command"
    );
    let p: SkillLoadedPayload = serde_json::from_value(events[0].payload.clone()).unwrap();
    assert_eq!(
        (p.name.as_str(), p.invoked_by),
        ("implement", Invoker::User)
    );
    let seen = seen.lock().unwrap();
    let ctx = &seen[0];
    assert!(texts(&ctx[ctx.len() - 2]).contains("# Implement"));
    assert_eq!(texts(&ctx[ctx.len() - 1]), "fix the off-by-one in cost.rs");
}

#[tokio::test]
async fn invoke_skill_with_no_arguments_and_unknown_skill() {
    let (mut rt, seen, _dir) = rig(vec![vec![text("ok"), done("stop")]]);
    let err = rt
        .invoke_skill(steve(), "nope", "", &mut |_| {})
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::UnknownSkill(ref n) if n == "nope"),
        "{err}"
    );
    assert!(rt.log().is_empty(), "nothing appended for an unknown skill");

    rt.invoke_skill(steve(), "tdd", "  ", &mut |_| {})
        .await
        .unwrap();
    let ctx = &seen.lock().unwrap()[0];
    assert_eq!(texts(&ctx[ctx.len() - 1]), "Run the `tdd` skill now.");
}
