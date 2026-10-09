//! Memory extraction in the loop (docs/PLAN-phase4.md section 9): a
//! scripted reply with one stated decision, one task instruction the
//! user also stated, one inferred fact and one line pointing at a tool
//! result; only the durable, stated decision lands, once, and the next
//! request's prefix carries it. `/remember` skips the model entirely.

mod common;

use aigentic_core::{ContentBlock, EventKind, Message, ProviderEvent, Role, ToolCall};
use aigentic_log::{MemoryExtractedPayload, MemoryHome, MemoryRememberedPayload, ThreadLog};
use aigentic_runtime::project::{MEMORY_DIR, person_memory_dir};
use aigentic_runtime::{
    GlobalLayer, Layers, MEMORY_PROMPT, MEMORY_REQUEST, PERSON_MEMORY_HEADING, Prices, Project,
    Runtime, RuntimeError, WorkspaceLayer,
};
use aigentic_tools::ToolRegistry;
use common::{EchoTool, Seen, done, scripted, steve, usage};
use serde_json::json;
use std::sync::{Arc, Mutex};

fn text(t: &str) -> ProviderEvent {
    ProviderEvent::TextDelta(t.into())
}

fn project_dir(memory_section: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("aigentic.toml"),
        format!("[project]\nname = \"m\"\n{memory_section}"),
    )
    .unwrap();
    dir
}

/// The fixture's workspace name, as its layer renders it.
const WORKSPACE: &str = "ws";

/// The project's memory folder, below its root.
const PROJECT_MEMORY: &str = ".aigentic/memory";

/// The config directory the fixture's person memory lives under, below
/// the project's temp dir so a test can reach the folder it wrote to.
fn config_dir(dir: &tempfile::TempDir) -> std::path::PathBuf {
    dir.path().join("config")
}

/// The shared directory the fixture's workspace layer was read from.
fn shared_dir(dir: &tempfile::TempDir) -> std::path::PathBuf {
    dir.path().join("shared")
}

/// The workspace's memory folder, as the fixture's `WorkspaceLayer`
/// looks it up.
fn workspace_memory(dir: &tempfile::TempDir) -> std::path::PathBuf {
    WorkspaceLayer::dir(&shared_dir(dir)).join(MEMORY_DIR)
}

/// The person's memory folder, as the fixture's `GlobalLayer` looks it
/// up.
fn person_memory(dir: &tempfile::TempDir) -> std::path::PathBuf {
    person_memory_dir(&config_dir(dir))
}

/// Which homes a fixture thread is built with: the person's folder, who
/// owns it, and whether a workspace is loaded. The project is always there.
#[derive(Clone, Copy)]
struct Homes {
    person_dir: bool,
    owner: Option<&'static str>,
    workspace: bool,
}

impl Homes {
    /// Every home the loader can give a thread.
    const ALL: Self = Self {
        person_dir: true,
        owner: Some(common::STEVE),
        workspace: true,
    };
}

/// The shared runtime shape: one thread provider, one model label, an
/// echo tool and the homes the caller asks for, the project's always.
fn runtime_without(
    dir: &tempfile::TempDir,
    provider: Box<dyn aigentic_core::Provider>,
    label: &str,
    homes: Homes,
) -> Runtime {
    let log = ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap();
    let project = Project::open(dir.path()).unwrap().unwrap();
    let registry: ToolRegistry =
        vec![Box::new(EchoTool(Arc::new(Mutex::new(Vec::new())))) as Box<dyn aigentic_core::Tool>]
            .into();
    let global = GlobalLayer::load(
        &config_dir(dir).join("instructions.md"),
        homes.person_dir.then(|| person_memory(dir)).as_deref(),
        homes.owner,
        Vec::new(),
        Vec::new(),
    )
    .unwrap();
    let workspace = if homes.workspace {
        WorkspaceLayer::load(WORKSPACE, Some(&shared_dir(dir)))
            .unwrap()
            .into()
    } else {
        None
    };
    Runtime::new(
        provider,
        registry,
        log,
        aigentic_core::AgentId("worker".into()),
    )
    .with_layers(Layers {
        global,
        workspace,
        project: Some(project),
    })
    .with_model_label(label)
}

/// The fixture thread with every home loaded.
fn runtime_with(
    dir: &tempfile::TempDir,
    provider: Box<dyn aigentic_core::Provider>,
    label: &str,
) -> Runtime {
    runtime_without(dir, provider, label, Homes::ALL)
}

fn rig(dir: &tempfile::TempDir, script: Vec<Vec<ProviderEvent>>) -> (Runtime, Seen) {
    let (provider, seen) = scripted(script);
    (runtime_with(dir, provider, "scripted"), seen)
}

fn rig_without(
    dir: &tempfile::TempDir,
    script: Vec<Vec<ProviderEvent>>,
    homes: Homes,
) -> (Runtime, Seen) {
    let (provider, seen) = scripted(script);
    (runtime_without(dir, provider, "scripted", homes), seen)
}

/// The file a kind tag writes to, from the table the writer uses.
fn memory_file(kind: &str) -> String {
    let (_, file) = aigentic_runtime::MEMORY_FILES
        .iter()
        .find(|(k, _)| *k == kind)
        .expect("a kind the table knows");
    (*file).to_owned()
}

/// `dir`'s file names, sorted.
fn file_names(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

async fn say(rt: &mut Runtime, what: &str) {
    rt.run_turn(steve(), vec![ContentBlock::Text(what.into())], &mut |_| {})
        .await
        .unwrap();
}

fn texts(m: &Message) -> String {
    m.blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

/// A turn: user (seq 0), assistant with a tool call (1), tool result (2),
/// assistant text (3), turn_ended (4).
fn one_turn() -> Vec<Vec<ProviderEvent>> {
    vec![
        vec![
            ProviderEvent::ToolCall(ToolCall {
                id: "c1".into(),
                name: "echo".into(),
                args: json!({"msg": "the tests are green"}),
            }),
            done("tool_use"),
        ],
        vec![text("Noted."), done("stop")],
    ]
}

/// The extraction reply: one decision the user stated, one task
/// instruction they also stated (issue #14), one fact the assistant
/// inferred (seq 3), one line pointing at the tool result (2).
const REPLY: &str = "decision @0 durable: Use Swedish in the UI.\n\
decision @0 task: Show the diff; don't commit.\n\
fact @3 durable: The user prefers short answers.\n\
fact @2 durable: The tests are green.\n";

#[tokio::test]
async fn only_the_stated_decision_lands_and_the_next_prefix_carries_it() {
    let dir = project_dir("");
    let mut script = one_turn();
    script.push(vec![text(REPLY), usage(300, 20)]);
    script.push(vec![text("ok"), done("stop")]);
    let (mut rt, seen) = rig(&dir, script);

    rt.run_turn(
        steve(),
        vec![ContentBlock::Text(
            "For the record, use Swedish in the UI.".into(),
        )],
        &mut |_| {},
    )
    .await
    .unwrap();
    let p = rt.extract_memory(&mut |_| {}).await.unwrap().unwrap();
    assert_eq!(p.through_seq, 4);
    assert_eq!(p.model, "scripted");
    assert_eq!(p.usage.input_tokens, 300);
    assert_eq!(p.written.len(), 1, "{:?}", p.written);
    assert_eq!(p.written[0].file, "decisions.md");
    assert_eq!(p.written[0].text, "Use Swedish in the UI.");
    assert_eq!(p.written[0].at_seq, 0);
    assert_eq!(p.written[0].stated_by, steve());

    // The extraction call saw the fixed prompt and a seq-tagged transcript.
    let request = seen.lock().unwrap()[2].clone();
    assert_eq!(request[0].role, Role::System);
    assert_eq!(texts(&request[0]), MEMORY_PROMPT);
    let transcript = texts(&request[1]);
    assert!(
        transcript.starts_with("[seq 0] user steve: For the record, use Swedish in the UI.\n[seq 1] assistant worker: [calls echo]\n[seq 2] tool_result: echo: the tests are green\n[seq 3] assistant worker: Noted.\n"),
        "{transcript}"
    );

    // The event is the audit.
    let events = rt.log().read_all().unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.kind, EventKind::MemoryExtracted);
    let back: MemoryExtractedPayload = serde_json::from_value(last.payload.clone()).unwrap();
    assert_eq!(back, p);

    // The file has the line with its provenance; nothing else.
    let mem = dir.path().join(".aigentic/memory");
    let decisions = std::fs::read_to_string(mem.join("decisions.md")).unwrap();
    assert!(
        decisions.starts_with("- Use Swedish in the UI. <!-- 20"),
        "{decisions}"
    );
    assert!(decisions.contains(&format!("thread {} -->\n", rt.log().thread_id())));
    assert!(!mem.join("facts.md").exists(), "inference is not filed");

    // The next turn's prefix carries it.
    rt.run_turn(
        steve(),
        vec![ContentBlock::Text("next".into())],
        &mut |_| {},
    )
    .await
    .unwrap();
    let ctx = &seen.lock().unwrap()[3];
    let memory = ctx
        .iter()
        .find(|m| texts(m).starts_with("# Project memory"))
        .expect("memory block in the prefix");
    assert!(texts(memory).contains("## decisions.md\n\n- Use Swedish in the UI."));
}

#[tokio::test]
async fn a_second_extraction_does_not_write_the_line_twice_and_moves_the_cursor() {
    let dir = project_dir("");
    let mut script = one_turn();
    script.push(vec![text(REPLY)]);
    script.extend(one_turn());
    // The model repeats the decision, now pointing at the new user message.
    script.push(vec![text("decision @6 durable: Use Swedish in the UI.\n")]);
    let (mut rt, _) = rig(&dir, script);
    say(&mut rt, "For the record, use Swedish in the UI.").await;
    let first = rt.extract_memory(&mut |_| {}).await.unwrap().unwrap();
    assert_eq!(first.written.len(), 1);
    say(&mut rt, "For the record, use Swedish in the UI.").await;
    let second = rt.extract_memory(&mut |_| {}).await.unwrap().unwrap();
    assert_eq!(second.through_seq, 10);
    assert!(second.written.is_empty(), "{:?}", second.written);
    let decisions =
        std::fs::read_to_string(dir.path().join(".aigentic/memory/decisions.md")).unwrap();
    assert_eq!(decisions.matches("Use Swedish").count(), 1);
    assert_eq!(decisions.lines().count(), 1);
}

#[tokio::test]
async fn a_hand_edited_file_changes_the_next_prefix() {
    let dir = project_dir("");
    let mut script = one_turn();
    script.push(vec![text(REPLY)]);
    script.push(vec![text("ok"), done("stop")]);
    let (mut rt, seen) = rig(&dir, script);
    rt.run_turn(
        steve(),
        vec![ContentBlock::Text(
            "For the record, use Swedish in the UI.".into(),
        )],
        &mut |_| {},
    )
    .await
    .unwrap();
    rt.extract_memory(&mut |_| {}).await.unwrap();
    let path = dir.path().join(".aigentic/memory/decisions.md");
    std::fs::write(&path, "- Use Finnish in the UI.\n").unwrap();
    // No explicit reload: the next turn re-reads the files itself.
    rt.run_turn(
        steve(),
        vec![ContentBlock::Text("next".into())],
        &mut |_| {},
    )
    .await
    .unwrap();
    let ctx = &seen.lock().unwrap()[3];
    let memory = ctx
        .iter()
        .find(|m| texts(m).starts_with("# Project memory"))
        .expect("memory block");
    assert!(texts(memory).contains("Use Finnish"));
    assert!(!texts(memory).contains("Use Swedish"));
}

/// A file edited by hand reaches the next turn's prefix in every home,
/// not only the project's.
#[tokio::test]
async fn a_turn_start_reloads_all_three_homes() {
    let dir = project_dir("");
    let (mut rt, seen) = rig(&dir, vec![vec![text("ok"), done("stop")]]);

    // All three folders appear after the runtime was built.
    for folder in [
        person_memory(&dir),
        workspace_memory(&dir),
        dir.path().join(".aigentic").join(MEMORY_DIR),
    ] {
        std::fs::create_dir_all(&folder).unwrap();
    }
    std::fs::write(
        person_memory(&dir).join("facts.md"),
        "- The person works from the terminal.\n",
    )
    .unwrap();
    std::fs::write(
        workspace_memory(&dir).join("constraints.md"),
        "- The fleet runs Debian.\n",
    )
    .unwrap();
    let project = dir
        .path()
        .join(".aigentic")
        .join(MEMORY_DIR)
        .join("decisions.md");
    std::fs::write(&project, "- We ship on Fridays.\n").unwrap();

    // No explicit reload: the turn start re-reads all three.
    say(&mut rt, "next").await;

    let ctx = seen.lock().unwrap()[0].clone();
    let blocks: Vec<String> = ctx.iter().map(texts).collect();
    let person = blocks
        .iter()
        .position(|b| b.starts_with(PERSON_MEMORY_HEADING))
        .expect("the person's block is in the prefix");
    assert!(
        blocks[person].contains("The person works from the terminal."),
        "{}",
        blocks[person]
    );
    // The workspace's and the project's blocks share one message, the
    // workspace's first.
    let memory = blocks
        .iter()
        .position(|b| b.starts_with(&format!("# Workspace memory ({WORKSPACE})")))
        .expect("the workspace's and the project's block is in the prefix");
    assert!(
        blocks[memory].contains("The fleet runs Debian."),
        "{}",
        blocks[memory]
    );
    let shared = blocks[memory]
        .find("The fleet runs Debian.")
        .expect("reload did not reach the prefix");
    let own = blocks[memory]
        .find("We ship on Fridays.")
        .expect("the project's memory did not reach the prefix");
    assert!(person < memory && shared < own, "{}", blocks[memory]);
}

/// A reply naming all three homes, every line stated in the turn's one
/// user message.
const HOMES_REPLY: &str = "person decision @0 durable: The person works from the terminal.\n\
workspace fact @0 durable: The fleet runs Debian.\n\
constraint @0 durable: We ship on Fridays.\n";

/// A reply whose one line is the person's own.
const PERSON_REPLY: &str = "person decision @0 durable: We ship from main.\n";

/// The script of one turn plus its extraction, then one more turn.
fn turn_and_extraction(reply: &str) -> Vec<Vec<ProviderEvent>> {
    let mut script = one_turn();
    script.push(vec![text(reply), usage(300, 20)]);
    script.push(vec![text("ok"), done("stop")]);
    script
}

#[tokio::test]
async fn an_extracted_line_lands_in_the_home_the_reply_names() {
    let dir = project_dir("");
    let (mut rt, _) = rig(&dir, turn_and_extraction(HOMES_REPLY));
    say(
        &mut rt,
        "For the record, I work from the terminal and the fleet runs Debian.",
    )
    .await;
    let p = rt.extract_memory(&mut |_| {}).await.unwrap().unwrap();

    let homes: Vec<MemoryHome> = p.written.iter().map(|l| l.home).collect();
    assert_eq!(
        homes,
        vec![
            MemoryHome::Person,
            MemoryHome::Workspace,
            MemoryHome::Project
        ],
        "{:?}",
        p.written
    );
    assert_eq!(p.written[0].file, memory_file("decision"));
    assert_eq!(p.written[1].file, memory_file("fact"));
    assert_eq!(p.written[2].file, memory_file("constraint"));

    let person =
        std::fs::read_to_string(person_memory(&dir).join(memory_file("decision"))).unwrap();
    assert!(
        person.contains("- The person works from the terminal."),
        "{person}"
    );
    let workspace =
        std::fs::read_to_string(workspace_memory(&dir).join(memory_file("fact"))).unwrap();
    assert!(
        workspace.contains("- The fleet runs Debian."),
        "{workspace}"
    );
    let project = std::fs::read_to_string(
        dir.path()
            .join(PROJECT_MEMORY)
            .join(memory_file("constraint")),
    )
    .unwrap();
    assert!(project.contains("- We ship on Fridays."), "{project}");

    // One file per home, and nothing in a folder that was not the line's.
    assert_eq!(
        file_names(&person_memory(&dir)),
        vec![memory_file("decision")]
    );
    assert_eq!(
        file_names(&workspace_memory(&dir)),
        vec![memory_file("fact")]
    );
    assert_eq!(
        file_names(&dir.path().join(PROJECT_MEMORY)),
        vec![memory_file("constraint")]
    );
}

#[tokio::test]
async fn the_prefix_carries_the_person_workspace_and_project_memory_in_that_order() {
    let dir = project_dir("");
    let (mut rt, seen) = rig(&dir, turn_and_extraction(HOMES_REPLY));
    say(
        &mut rt,
        "For the record, I work from the terminal and the fleet runs Debian.",
    )
    .await;
    rt.extract_memory(&mut |_| {}).await.unwrap().unwrap();
    say(&mut rt, "next").await;

    let ctx = seen.lock().unwrap()[3].clone();
    let blocks: Vec<String> = ctx.iter().map(texts).collect();
    let person = blocks
        .iter()
        .position(|b| b.starts_with(PERSON_MEMORY_HEADING))
        .expect("the person's block is in the prefix");
    assert!(
        blocks[person].contains("The person works from the terminal."),
        "{}",
        blocks[person]
    );
    assert!(
        !blocks[person].contains("# Project memory") && !blocks[person].contains("# Workspace"),
        "the person's memory is its own block: {}",
        blocks[person]
    );
    // The workspace's and the project's share one message, the workspace's
    // first, exactly where they were before the person's home existed.
    let memory = blocks
        .iter()
        .position(|b| b.starts_with(&format!("# Workspace memory ({WORKSPACE})")))
        .expect("the workspace's and the project's block is in the prefix");
    let shared = blocks[memory]
        .find("The fleet runs Debian.")
        .expect("the workspace's memory is in the block");
    let own = blocks[memory]
        .find("# Project memory")
        .expect("the project's memory is in the block");
    assert!(shared < own, "{}", blocks[memory]);
    assert!(blocks[memory].contains("We ship on Fridays."));
    assert!(person < memory, "the person's block comes first");
    // `Layers::memory_prefix` is the workspace's and the project's only.
    let prefix = rt.layers().memory_prefix().unwrap();
    assert!(!prefix.contains(PERSON_MEMORY_HEADING), "{prefix}");
    assert!(
        !prefix.contains("The person works from the terminal."),
        "{prefix}"
    );
}

#[tokio::test]
async fn a_restating_line_is_not_filed_again_in_another_home() {
    let dir = project_dir("");
    let mut script = turn_and_extraction(PERSON_REPLY);
    script.push(vec![text("ok"), done("stop")]);
    script.push(vec![
        text("fact @6 durable: We ship from main.\n"),
        usage(300, 20),
    ]);
    let (mut rt, _) = rig(&dir, script);

    say(&mut rt, "For the record, we ship from main.").await;
    let first = rt.extract_memory(&mut |_| {}).await.unwrap().unwrap();
    assert_eq!(first.written.len(), 1, "{:?}", first.written);
    assert_eq!(first.written[0].home, MemoryHome::Person);

    // The same sentence again, as a project fact: it is already memory.
    say(&mut rt, "For the record, we ship from main.").await;
    let second = rt.extract_memory(&mut |_| {}).await.unwrap().unwrap();
    assert!(second.written.is_empty(), "{:?}", second.written);
    assert!(
        !dir.path()
            .join(PROJECT_MEMORY)
            .join(memory_file("fact"))
            .exists(),
        "a restatement is not re-filed in the project's folder"
    );
}

#[tokio::test]
async fn a_reply_that_names_an_unavailable_home_lands_nowhere() {
    let dir = project_dir("");
    let (mut rt, _) = rig_without(
        &dir,
        turn_and_extraction(
            "workspace fact @0 durable: The fleet runs Debian.\ndecision @0 durable: We ship from main.\n",
        ),
        Homes {
            person_dir: true,
            owner: Some(common::STEVE),
            workspace: false,
        },
    );
    say(
        &mut rt,
        "For the record, the fleet runs Debian and we ship from main.",
    )
    .await;
    let p = rt.extract_memory(&mut |_| {}).await.unwrap().unwrap();

    assert_eq!(p.written.len(), 1, "{:?}", p.written);
    assert_eq!(p.written[0].home, MemoryHome::Project);
    assert!(!shared_dir(&dir).exists(), "nothing outside the project's");
    assert!(
        !dir.path()
            .join(PROJECT_MEMORY)
            .join(memory_file("fact"))
            .exists()
    );
}

#[tokio::test]
async fn a_config_dir_with_no_person_facts_leaves_the_request_untouched() {
    let dir = project_dir("");
    // A person's folder holding no fact (whitespace only) and a workspace
    // with no memory file: neither block reaches the prefix.
    std::fs::create_dir_all(person_memory(&dir)).unwrap();
    std::fs::write(person_memory(&dir).join(memory_file("fact")), "  \n\n").unwrap();
    let script = || {
        let mut script = one_turn();
        script.push(vec![text("none\n"), usage(300, 20)]);
        script.push(vec![text("ok"), done("stop")]);
        script
    };
    let (mut with_homes, seen) = rig(&dir, script());
    let (mut plain, plain_seen) = rig_without(
        &dir,
        script(),
        Homes {
            person_dir: false,
            owner: None,
            workspace: false,
        },
    );

    for rt in [&mut with_homes, &mut plain] {
        say(rt, "For the record, we ship from main.").await;
        rt.extract_memory(&mut |_| {}).await.unwrap().unwrap();
        say(rt, "next").await;
    }

    let blocks: Vec<String> = seen.lock().unwrap()[3].iter().map(texts).collect();
    let plain_blocks: Vec<String> = plain_seen.lock().unwrap()[3].iter().map(texts).collect();
    assert_eq!(blocks, plain_blocks, "a home with no fact moved the prefix");
    assert!(
        blocks.iter().all(|b| !b.contains(PERSON_MEMORY_HEADING)),
        "{blocks:?}"
    );
    // Nothing was refiled into the person's folder either.
    assert_eq!(
        std::fs::read_to_string(person_memory(&dir).join(memory_file("fact"))).unwrap(),
        "  \n\n"
    );
    // With no home to offer, the request is the one a project-only thread
    // sends.
    let (mut none_at_all, none_seen) = rig_without(
        &dir,
        script(),
        Homes {
            person_dir: false,
            owner: None,
            workspace: false,
        },
    );
    say(&mut none_at_all, "For the record, we ship from main.").await;
    none_at_all
        .extract_memory(&mut |_| {})
        .await
        .unwrap()
        .unwrap();
    assert_eq!(texts(&none_seen.lock().unwrap()[2][2]), MEMORY_REQUEST);
}

#[tokio::test]
async fn the_extraction_request_offers_only_the_homes_this_thread_has() {
    let dir = project_dir("");
    let (mut rt, seen) = rig(&dir, turn_and_extraction(HOMES_REPLY));
    say(&mut rt, "For the record, we ship from main.").await;
    rt.extract_memory(&mut |_| {}).await.unwrap().unwrap();
    let request = texts(&seen.lock().unwrap()[2][2]);
    let person = request
        .find(&format!("`{}`", "person"))
        .expect("the person's home is offered");
    let workspace = request
        .find(&format!("`{}`", "workspace"))
        .expect("the workspace's home is offered");
    assert!(person < workspace, "{request}");
    assert!(request.contains("before the kind"), "{request}");

    // A person's folder with no owner is not the person's home.
    let (mut owned, seen) = rig_without(
        &dir,
        turn_and_extraction(HOMES_REPLY),
        Homes {
            person_dir: true,
            owner: None,
            workspace: true,
        },
    );
    say(&mut owned, "For the record, we ship from main.").await;
    owned.extract_memory(&mut |_| {}).await.unwrap().unwrap();
    let request = texts(&seen.lock().unwrap()[2][2]);
    assert!(!request.contains("`person`"), "{request}");
    assert!(request.contains("`workspace`"), "{request}");

    // No home beyond the project's: the request is today's.
    let (mut project_only, seen) = rig_without(
        &dir,
        turn_and_extraction(HOMES_REPLY),
        Homes {
            person_dir: false,
            owner: None,
            workspace: false,
        },
    );
    say(&mut project_only, "For the record, we ship from main.").await;
    project_only
        .extract_memory(&mut |_| {})
        .await
        .unwrap()
        .unwrap();
    assert_eq!(texts(&seen.lock().unwrap()[2][2]), MEMORY_REQUEST);
}

#[tokio::test]
async fn an_owner_stated_person_line_lands_in_the_person_memory() {
    let dir = project_dir("");
    let (mut rt, _) = rig(&dir, turn_and_extraction(PERSON_REPLY));
    say(&mut rt, "For the record, we ship from main.").await;
    let p = rt.extract_memory(&mut |_| {}).await.unwrap().unwrap();

    assert_eq!(p.written.len(), 1, "{:?}", p.written);
    assert_eq!(p.written[0].home, MemoryHome::Person);
    let person =
        std::fs::read_to_string(person_memory(&dir).join(memory_file("decision"))).unwrap();
    assert!(person.contains("- We ship from main."), "{person}");
}

#[tokio::test]
async fn a_person_line_the_owner_did_not_state_is_dropped() {
    let dir = project_dir("");
    let (mut rt, _) = rig(&dir, turn_and_extraction(PERSON_REPLY));
    rt.run_turn(
        common::magnus(),
        vec![ContentBlock::Text(
            "For the record, we ship from main.".into(),
        )],
        &mut |_| {},
    )
    .await
    .unwrap();
    let p = rt.extract_memory(&mut |_| {}).await.unwrap().unwrap();

    assert!(p.written.is_empty(), "{:?}", p.written);
    assert!(
        !person_memory(&dir).exists(),
        "nothing in the person's folder"
    );
    assert!(
        !dir.path()
            .join(PROJECT_MEMORY)
            .join(memory_file("decision"))
            .exists(),
        "a dropped person line is never refiled"
    );
}

#[tokio::test]
async fn every_n_turns_two_skips_a_turn_and_disabled_never_runs() {
    let dir = project_dir("[memory]\nevery_n_turns = 2\n");
    let mut script = one_turn();
    script.extend(one_turn());
    script.push(vec![text("decision @0 durable: Use Swedish in the UI.\n")]);
    let (mut rt, seen) = rig(&dir, script);
    rt.run_turn(
        steve(),
        vec![ContentBlock::Text(
            "For the record, use Swedish in the UI.".into(),
        )],
        &mut |_| {},
    )
    .await
    .unwrap();
    assert!(rt.extract_memory(&mut |_| {}).await.unwrap().is_none());
    assert_eq!(seen.lock().unwrap().len(), 2, "no extraction call was made");
    rt.run_turn(
        steve(),
        vec![ContentBlock::Text("again".into())],
        &mut |_| {},
    )
    .await
    .unwrap();
    let p = rt.extract_memory(&mut |_| {}).await.unwrap().unwrap();
    assert_eq!(p.through_seq, 9, "both turns were considered");
    assert_eq!(p.written.len(), 1);

    let dir = project_dir("[memory]\nenabled = false\n");
    let (mut rt, seen) = rig(&dir, one_turn());
    rt.run_turn(steve(), vec![ContentBlock::Text("hi".into())], &mut |_| {})
        .await
        .unwrap();
    assert!(rt.extract_memory(&mut |_| {}).await.unwrap().is_none());
    assert_eq!(seen.lock().unwrap().len(), 2);
    assert!(!dir.path().join(".aigentic/memory").exists());
}

#[tokio::test]
async fn a_turn_that_did_not_end_done_is_not_extracted_on_its_own() {
    let dir = project_dir("");
    let (mut rt, seen) = rig(
        &dir,
        vec![vec![ProviderEvent::Error(
            aigentic_core::ProviderError::Transport("boom".into()),
        )]],
    );
    let _ = rt
        .run_turn(steve(), vec![ContentBlock::Text("hi".into())], &mut |_| {})
        .await;
    assert!(rt.extract_memory(&mut |_| {}).await.unwrap().is_none());
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn remember_files_a_line_directly_without_a_model_call() {
    let dir = project_dir("");
    let (mut rt, seen) = rig(&dir, one_turn());

    rt.remember(steve(), "decision We ship on Fridays", &mut |_| {})
        .unwrap();

    // One event so far: the audit, whose seq the line itself points at.
    let events = rt.log().read_all().unwrap();
    let [e] = &events[..] else {
        panic!("expected one event, got {}", events.len())
    };
    assert_eq!(e.kind, EventKind::MemoryRemembered);
    assert_eq!(e.author, steve());
    let p: MemoryRememberedPayload = serde_json::from_value(e.payload.clone()).unwrap();
    assert_eq!(p.file, "decisions.md");
    assert_eq!(p.text, "We ship on Fridays");
    assert!(p.written);
    let mem = dir.path().join(".aigentic/memory");
    let decisions = std::fs::read_to_string(mem.join("decisions.md")).unwrap();
    assert_eq!(decisions.lines().count(), 1);
    assert!(decisions.contains("- We ship on Fridays"), "{decisions}");

    // The same line again: no duplicate, and the event says so.
    rt.remember(steve(), "decision We ship on Fridays", &mut |_| {})
        .unwrap();
    let events = rt.log().read_all().unwrap();
    let p: MemoryRememberedPayload = serde_json::from_value(events[1].payload.clone()).unwrap();
    assert!(!p.written);
    let decisions = std::fs::read_to_string(mem.join("decisions.md")).unwrap();
    assert_eq!(decisions.lines().count(), 1);

    // No kind word: the line is a fact.
    rt.remember(steve(), "The project is called aigentic.", &mut |_| {})
        .unwrap();
    assert!(mem.join("facts.md").exists());

    // The next turn's prefix carries both, with no provider call spent:
    // the turn is the only thing that talked to the model.
    rt.run_turn(
        steve(),
        vec![ContentBlock::Text("next".into())],
        &mut |_| {},
    )
    .await
    .unwrap();
    let ctx = seen.lock().unwrap()[0].clone();
    let memory = ctx
        .iter()
        .find(|m| texts(m).starts_with("# Project memory"))
        .expect("memory block in the prefix");
    assert!(
        texts(memory).contains("## decisions.md\n\n- We ship on Fridays"),
        "{}",
        texts(memory)
    );
    assert!(texts(memory).contains("- The project is called aigentic."));
    assert_eq!(
        seen.lock().unwrap().len(),
        2,
        "the turn made both provider calls; /remember made none"
    );
}

/// The `memory_remembered` event a `/remember` call appended, last.
fn last_remembered(rt: &Runtime) -> MemoryRememberedPayload {
    let events = rt.log().read_all().unwrap();
    let e = events
        .iter()
        .rev()
        .find(|e| e.kind == EventKind::MemoryRemembered)
        .expect("a memory_remembered event");
    serde_json::from_value(e.payload.clone()).unwrap()
}

#[tokio::test]
async fn remember_takes_a_leading_home_word_and_reports_it() {
    let dir = project_dir("");
    let (mut rt, _) = rig(&dir, one_turn());

    rt.remember(steve(), "person decision We ship from main", &mut |_| {})
        .unwrap();
    let p = last_remembered(&rt);
    assert_eq!(p.home, MemoryHome::Person);
    assert_eq!(p.file, memory_file("decision"));
    assert_eq!(p.text, "We ship from main");
    assert!(p.written);
    let person =
        std::fs::read_to_string(person_memory(&dir).join(memory_file("decision"))).unwrap();
    assert!(person.contains("- We ship from main"), "{person}");

    rt.remember(steve(), "workspace fact The fleet runs Debian", &mut |_| {})
        .unwrap();
    let p = last_remembered(&rt);
    assert_eq!(p.home, MemoryHome::Workspace);
    assert_eq!(p.file, memory_file("fact"));
    let shared = std::fs::read_to_string(workspace_memory(&dir).join(memory_file("fact"))).unwrap();
    assert!(shared.contains("- The fleet runs Debian"), "{shared}");

    // A kind word without a home word is still the project's.
    rt.remember(steve(), "decision We ship on Fridays", &mut |_| {})
        .unwrap();
    let p = last_remembered(&rt);
    assert_eq!(p.home, MemoryHome::Project);
    assert_eq!(p.file, memory_file("decision"));

    // A home word with no kind word after it is the whole line, a project
    // fact.
    rt.remember(steve(), "workspace is noisy this week", &mut |_| {})
        .unwrap();
    let p = last_remembered(&rt);
    assert_eq!(p.home, MemoryHome::Project);
    assert_eq!(p.file, memory_file("fact"));
    assert_eq!(p.text, "workspace is noisy this week");
    let facts =
        std::fs::read_to_string(dir.path().join(PROJECT_MEMORY).join(memory_file("fact"))).unwrap();
    assert!(facts.contains("- workspace is noisy this week"), "{facts}");

    // And so is a kind word with nothing after it.
    rt.remember(steve(), "person fact", &mut |_| {}).unwrap();
    let p = last_remembered(&rt);
    assert_eq!(p.home, MemoryHome::Project);
    assert_eq!(p.file, memory_file("fact"));
    assert_eq!(p.text, "person fact");
    assert!(
        !workspace_memory(&dir)
            .join(memory_file("decision"))
            .exists()
    );
}

#[tokio::test]
async fn remember_names_a_home_this_thread_has_not_got() {
    // No workspace loaded.
    let dir = project_dir("");
    let (mut rt, _) = rig_without(
        &dir,
        one_turn(),
        Homes {
            person_dir: true,
            owner: Some(common::STEVE),
            workspace: false,
        },
    );
    let err = rt
        .remember(steve(), "workspace fact The fleet runs Debian", &mut |_| {})
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::NoMemoryHome(MemoryHome::Workspace)),
        "{err}"
    );
    assert!(!shared_dir(&dir).exists());

    // A person's folder, but no owner: nobody's home.
    let dir = project_dir("");
    let (mut rt, _) = rig_without(
        &dir,
        one_turn(),
        Homes {
            person_dir: true,
            owner: None,
            workspace: true,
        },
    );
    let err = rt
        .remember(steve(), "person fact We ship from main", &mut |_| {})
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::NoMemoryHome(MemoryHome::Person)),
        "{err}"
    );
    assert!(!person_memory(&dir).exists(), "nothing was written");
}

#[tokio::test]
async fn remember_person_is_refused_for_anyone_but_the_owner() {
    let dir = project_dir("");
    let (mut rt, _) = rig(&dir, one_turn());
    let err = rt
        .remember(
            common::magnus(),
            "person fact We ship from main",
            &mut |_| {},
        )
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::NoMemoryHome(MemoryHome::Person)),
        "{err}"
    );
    assert!(!person_memory(&dir).exists(), "nothing was written");

    // Nobody owns the home, so nobody may file there.
    let dir = project_dir("");
    let (mut rt, _) = rig_without(
        &dir,
        one_turn(),
        Homes {
            person_dir: true,
            owner: None,
            workspace: true,
        },
    );
    let err = rt
        .remember(steve(), "person fact We ship from main", &mut |_| {})
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::NoMemoryHome(MemoryHome::Person)),
        "{err}"
    );
}

#[test]
fn remember_without_a_project_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let (provider, _) = scripted(vec![]);
    let log = ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap();
    let registry: ToolRegistry = Vec::<Box<dyn aigentic_core::Tool>>::new().into();
    let mut rt = Runtime::new(
        provider,
        registry,
        log,
        aigentic_core::AgentId("worker".into()),
    );
    assert!(matches!(
        rt.remember(steve(), "anything at all", &mut |_| {}),
        Err(RuntimeError::NoProject)
    ));
}

#[tokio::test]
async fn for_the_record_we_deploy_from_main_only_files_exactly_one_line() {
    let dir = project_dir("");
    let mut script = one_turn();
    script.push(vec![text(
        "decision @0 durable: We deploy from main only.\n",
    )]);
    let (mut rt, seen) = rig(&dir, script);
    rt.run_turn(
        steve(),
        vec![ContentBlock::Text(
            "For the record, we deploy from main only.".into(),
        )],
        &mut |_| {},
    )
    .await
    .unwrap();
    let p = rt.extract_memory(&mut |_| {}).await.unwrap().unwrap();
    assert_eq!(p.written.len(), 1, "{:?}", p.written);
    assert_eq!(p.written[0].file, "decisions.md");
    assert_eq!(p.written[0].text, "We deploy from main only.");
    // The cue passed it to the model: the transcript carries the message.
    let request = seen.lock().unwrap()[2].clone();
    assert!(
        texts(&request[1])
            .contains("[seq 0] user steve: For the record, we deploy from main only."),
        "{}",
        texts(&request[1])
    );
    let decisions =
        std::fs::read_to_string(dir.path().join(".aigentic/memory/decisions.md")).unwrap();
    assert_eq!(
        decisions.matches("We deploy from main only.").count(),
        1,
        "{decisions}"
    );
    assert_eq!(decisions.lines().count(), 1, "{decisions}");
}

#[tokio::test]
async fn from_now_on_with_a_subject_files_exactly_one_line() {
    let dir = project_dir("");
    let mut script = one_turn();
    script.push(vec![text("fact @0 durable: Dates in Swedish format.\n")]);
    let (mut rt, _) = rig(&dir, script);
    rt.run_turn(
        steve(),
        vec![ContentBlock::Text(
            "From now on, we write dates in Swedish format.".into(),
        )],
        &mut |_| {},
    )
    .await
    .unwrap();
    let p = rt.extract_memory(&mut |_| {}).await.unwrap().unwrap();
    assert_eq!(p.written.len(), 1, "{:?}", p.written);
    assert_eq!(p.written[0].file, "facts.md");
    assert_eq!(p.written[0].text, "Dates in Swedish format.");
    let facts = std::fs::read_to_string(dir.path().join(".aigentic/memory/facts.md")).unwrap();
    assert_eq!(
        facts.matches("Dates in Swedish format.").count(),
        1,
        "{facts}"
    );
    assert_eq!(facts.lines().count(), 1, "{facts}");
}

/// Issue #14, reopened: a long task message is not memory, but the
/// one sentence the person opened with "For the record" is — and it
/// is the only sentence the model is offered.
#[tokio::test]
async fn a_long_message_with_one_for_the_record_sentence_files_only_that_sentence() {
    let dir = project_dir("");
    let mut script = one_turn();
    script.push(vec![text(
        "decision @0 durable: We deploy from main only.\n",
    )]);
    let (mut rt, seen) = rig(&dir, script);
    let message = "Ship #12 today. Run the full gate locally first: cargo fmt, \
cargo clippy --all-targets -- -D warnings, then cargo test. Fix whatever falls out \
and add no new dependencies. Then rewrite the README opening: three sentences \
without dashes, a short list of what works today, and Getting started covering \
install, config.toml with a profile, the key in .env, aigentic doctor, aigentic \
in a repo, and aigentic init for a new project. Drop the phase table and the \
status narrative, linking docs/PRD.md and AGENTS.md for depth instead. For the \
record, we deploy from main only. Label the bugs section next, cross-reference \
items 2 and 3 in their bodies, then push everything as one commit and comment on \
the issue with what changed.";
    assert!(message.chars().count() > 600);
    rt.run_turn(
        steve(),
        vec![ContentBlock::Text(message.into())],
        &mut |_| {},
    )
    .await
    .unwrap();
    let p = rt.extract_memory(&mut |_| {}).await.unwrap().unwrap();
    assert_eq!(p.written.len(), 1, "{:?}", p.written);
    assert_eq!(p.written[0].file, "decisions.md");
    assert_eq!(p.written[0].text, "We deploy from main only.");
    // The model was offered the one sentence, and nothing else of the
    // spec.
    let request = seen.lock().unwrap()[2].clone();
    let transcript = texts(&request[1]);
    assert!(
        transcript.contains("[seq 0] user steve: For the record, we deploy from main only.\n"),
        "{transcript}"
    );
    assert!(!transcript.contains("README"), "{transcript}");
    assert!(!transcript.contains("cargo fmt"), "{transcript}");
    let decisions =
        std::fs::read_to_string(dir.path().join(".aigentic/memory/decisions.md")).unwrap();
    assert_eq!(
        decisions.matches("We deploy from main only.").count(),
        1,
        "{decisions}"
    );
    assert_eq!(decisions.lines().count(), 1, "{decisions}");
}

/// The primary gate (issue #14): a user message with no cue is skipped
/// before the model sees it, and a line the model files anyway is
/// dropped after it.
#[tokio::test]
async fn an_instruction_without_a_cue_never_reaches_the_model_or_the_file() {
    let dir = project_dir("");
    let mut script = one_turn();
    script.push(vec![text(
        "decision @0 durable: Create ~/Projects/aigentic-web with Next.js + shadcn.\n",
    )]);
    let (mut rt, seen) = rig(&dir, script);
    rt.run_turn(
        steve(),
        vec![ContentBlock::Text(
            "Create ~/Projects/aigentic-web with Next.js + shadcn.".into(),
        )],
        &mut |_| {},
    )
    .await
    .unwrap();
    let p = rt.extract_memory(&mut |_| {}).await.unwrap().unwrap();
    assert!(p.written.is_empty(), "{:?}", p.written);
    // The model's transcript carried no user entry at all.
    let request = seen.lock().unwrap()[2].clone();
    assert!(
        !texts(&request[1]).contains("aigentic-web"),
        "{}",
        texts(&request[1])
    );
    assert!(!dir.path().join(".aigentic/memory").exists());
}

/// Issue #18: `memory_extracted.model` names the provider that ran the
/// extraction — the utility profile's model, not the thread's.
#[tokio::test]
async fn the_extraction_label_names_the_utility_model_not_the_threads() {
    let dir = project_dir("");
    let (thread, thread_seen) = scripted(one_turn());
    let (utility, utility_seen) = scripted(vec![vec![text(REPLY), usage(300, 20)]]);
    let mut rt =
        runtime_with(&dir, thread, "thread-model").with_utility(utility, "utility-model", None);

    say(&mut rt, "For the record, use Swedish in the UI.").await;
    let p = rt.extract_memory(&mut |_| {}).await.unwrap().unwrap();
    assert_eq!(p.model, "utility-model");
    assert!(!p.written.is_empty(), "{:?}", p.written);

    // The extraction reached the utility, so the label is not right by
    // accident, and never the thread's provider.
    let utility_seen = utility_seen.lock().unwrap();
    assert_eq!(utility_seen.len(), 1);
    assert_eq!(texts(&utility_seen[0][0]), MEMORY_PROMPT);
    assert!(
        thread_seen
            .lock()
            .unwrap()
            .iter()
            .all(|r| texts(&r[0]) != MEMORY_PROMPT),
        "the thread's provider saw the extraction prompt"
    );
}

/// Issue #46: the extraction is a priced call of its own. With a utility
/// profile that has a `[prices]` table, its usage line carries what that
/// table says the call cost — recomputed here from the same table and the
/// scripted usage, never a literal.
#[tokio::test]
async fn a_priced_utility_stamps_the_extraction_with_its_own_table() {
    let prices = Prices {
        input: 0.07,
        cache_read: 0.01,
        cache_write: 0.08,
        output: 0.28,
    };
    let dir = project_dir("");
    let (thread, _thread_seen) = scripted(one_turn());
    let (utility, _utility_seen) = scripted(vec![vec![text(REPLY), usage(300, 20)]]);
    let mut rt = runtime_with(&dir, thread, "thread-model").with_utility(
        utility,
        "utility-model",
        Some(prices),
    );

    say(&mut rt, "For the record, use Swedish in the UI.").await;
    let p = rt.extract_memory(&mut |_| {}).await.unwrap().unwrap();

    // `usage(300, 20)`: 300 input at 0.07 and 20 output at 0.28 per
    // million, from the table the test passed in.
    let expected = prices.cost_usd(&p.usage);
    assert!(expected > 0.0, "{expected}");
    assert_eq!(p.usage.cost_usd, Some(expected));
    assert_eq!((p.usage.input_tokens, p.usage.output_tokens), (300, 20));
    assert!(!p.usage.estimated, "the provider reported this usage");
}

/// Issue #46, the fallback half: with no utility profile the thread's own
/// provider ran the extraction, so the thread's table prices it.
#[tokio::test]
async fn a_threads_table_prices_an_extraction_the_thread_ran() {
    let prices = Prices {
        input: 0.11,
        cache_read: 0.0,
        cache_write: 0.0,
        output: 0.44,
    };
    let dir = project_dir("");
    // The thread's own provider is the extractor here, so its script
    // holds the extraction exchange too.
    let mut script = one_turn();
    script.push(vec![text(REPLY), usage(300, 20)]);
    let (thread, _seen) = scripted(script);
    let mut rt =
        runtime_with(&dir, thread, "thread-model").with_pricing("thread-profile", Some(prices));

    say(&mut rt, "For the record, use Swedish in the UI.").await;
    let p = rt.extract_memory(&mut |_| {}).await.unwrap().unwrap();

    assert_eq!(p.usage.cost_usd, Some(prices.cost_usd(&p.usage)));
    assert!(p.usage.cost_usd.unwrap() > 0.0);
}

/// Issue #46, the honest hole: a utility profile with no table of its own
/// does not borrow the thread's prices for its calls — the line stays
/// unpriced, on a model name the report can price later.
#[tokio::test]
async fn a_utility_without_prices_leaves_the_extraction_unpriced() {
    let thread_prices = Prices {
        input: 9.0,
        cache_read: 0.0,
        cache_write: 0.0,
        output: 9.0,
    };
    let dir = project_dir("");
    let (thread, _thread_seen) = scripted(one_turn());
    let (utility, _utility_seen) = scripted(vec![vec![text(REPLY), usage(300, 20)]]);
    let mut rt = runtime_with(&dir, thread, "thread-model")
        .with_utility(utility, "utility-model", None)
        .with_pricing("thread-profile", Some(thread_prices));

    say(&mut rt, "For the record, use Swedish in the UI.").await;
    let p = rt.extract_memory(&mut |_| {}).await.unwrap().unwrap();

    assert_eq!(p.usage.cost_usd, None);
    assert_eq!(p.model, "utility-model");
}

/// T6, issue #52: extraction runs on the utility provider — a different
/// model with a different tokenizer — so its numbers must never move the
/// thread's ratio. The utility here reports the most absurd count a wrong
/// sample could give, and the thread's own turn (which reports no usage)
/// leaves the ratio at its seed.
#[tokio::test]
async fn the_utility_model_never_calibrates_the_thread() {
    let dir = project_dir("");
    let (thread, _thread_seen) = scripted(one_turn());
    let (utility, utility_seen) = scripted(vec![vec![text(REPLY), usage(9_999_999, 1)]]);
    let mut rt =
        runtime_with(&dir, thread, "thread-model").with_utility(utility, "utility-model", None);

    say(&mut rt, "For the record, use Swedish in the UI.").await;
    assert_eq!(
        rt.eviction_ratio(),
        1.0,
        "the thread's own turn reported nothing"
    );

    let extracted = rt.extract_memory(&mut |_| {}).await.unwrap();
    assert!(extracted.is_some(), "the extraction ran");
    assert_eq!(utility_seen.lock().unwrap().len(), 1, "on the utility");
    assert_eq!(
        rt.eviction_ratio(),
        1.0,
        "an absurd utility count moved the thread's ratio"
    );
}
