//! Memory extraction in the loop (docs/PLAN-phase4.md section 9): a
//! scripted reply with one stated decision, one inferred fact and one
//! line pointing at a tool result; only the decision lands, once, and the
//! next request's prefix carries it.

mod common;

use aigentic_core::{ContentBlock, EventKind, Message, ProviderEvent, Role, ToolCall};
use aigentic_log::{MemoryExtractedPayload, ThreadLog};
use aigentic_runtime::{Layers, MEMORY_PROMPT, Project, Runtime};
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

fn rig(dir: &tempfile::TempDir, script: Vec<Vec<ProviderEvent>>) -> (Runtime, Seen) {
    let (provider, seen) = scripted(script);
    let log = ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap();
    let project = Project::open(dir.path()).unwrap().unwrap();
    let registry: ToolRegistry =
        vec![Box::new(EchoTool(Arc::new(Mutex::new(Vec::new())))) as Box<dyn aigentic_core::Tool>]
            .into();
    let runtime = Runtime::new(
        provider,
        registry,
        log,
        aigentic_core::AgentId("worker".into()),
    )
    .with_layers(Layers::default().with_project(project))
    .with_model_label("scripted");
    (runtime, seen)
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

/// The extraction reply: one decision the user stated, one fact the
/// assistant inferred (seq 3), one line pointing at the tool result (2).
const REPLY: &str = "decision @0: Use Swedish in the UI.\n\
fact @3: The user prefers short answers.\n\
fact @2: The tests are green.\n";

#[tokio::test]
async fn only_the_stated_decision_lands_and_the_next_prefix_carries_it() {
    let dir = project_dir("");
    let mut script = one_turn();
    script.push(vec![text(REPLY), usage(300, 20)]);
    script.push(vec![text("ok"), done("stop")]);
    let (mut rt, seen) = rig(&dir, script);

    rt.run_turn(
        steve(),
        vec![ContentBlock::Text("Use Swedish in the UI.".into())],
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
        transcript.starts_with("[seq 0] user steve: Use Swedish in the UI.\n[seq 1] assistant worker: [calls echo]\n[seq 2] tool_result: echo: the tests are green\n[seq 3] assistant worker: Noted.\n"),
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
    script.push(vec![text("decision @6: Use Swedish in the UI.\n")]);
    let (mut rt, _) = rig(&dir, script);
    say(&mut rt, "Use Swedish in the UI.").await;
    let first = rt.extract_memory(&mut |_| {}).await.unwrap().unwrap();
    assert_eq!(first.written.len(), 1);
    say(&mut rt, "Use Swedish in the UI.").await;
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
        vec![ContentBlock::Text("Use Swedish in the UI.".into())],
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

#[tokio::test]
async fn every_n_turns_two_skips_a_turn_and_disabled_never_runs() {
    let dir = project_dir("[memory]\nevery_n_turns = 2\n");
    let mut script = one_turn();
    script.extend(one_turn());
    script.push(vec![text("decision @0: Use Swedish in the UI.\n")]);
    let (mut rt, seen) = rig(&dir, script);
    rt.run_turn(
        steve(),
        vec![ContentBlock::Text("Use Swedish in the UI.".into())],
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
