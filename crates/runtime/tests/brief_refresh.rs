//! A project's brief keeps its current state fresh: after a batch of
//! turns that wrote files, one utility call rewrites the brief's
//! `## Current state` section and a `brief_refreshed` event records it.
//! A batch that wrote nothing, a project with no brief and a project
//! that turned refreshing off cost nothing.

mod common;

use aigentic_core::{Author, ContentBlock, EventKind, Message, ProviderEvent};
use aigentic_log::{BriefRefreshedPayload, NewEvent, ThreadLog};
use aigentic_runtime::brief::project_brief_path;
use aigentic_runtime::{GlobalLayer, Layers, Prices, Project, Runtime};
use aigentic_tools::ToolRegistry;
use common::{Seen, scripted, usage};
use serde_json::json;
use ulid::Ulid;

const BRIEF: &str =
    "# m\n\nA harness.\n\n## Current state\n\nNothing yet.\n\n## Notes\n\nKeep this.\n";
const REPLY: &str = "The jobs lane is merged and the briefs lane has started.";
const EARLY: &str = "the turn before the last refresh";
const LATE: &str = "the turn after the last refresh";

/// A project `m` with `brief.md` when `brief` is set and `section` in its
/// `aigentic.toml`.
fn project(brief: bool, section: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("aigentic.toml"),
        format!("[project]\nname = \"m\"\n{section}"),
    )
    .unwrap();
    if brief {
        let path = project_brief_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, BRIEF).unwrap();
    }
    dir
}

/// One turn in the log: the person's text, a reply, and its end with the
/// files it wrote.
fn turn(said: &str, touched: &[&str]) -> Vec<(EventKind, serde_json::Value)> {
    vec![
        (
            EventKind::UserMessage,
            json!({"blocks": [{"type": "text", "text": said}]}),
        ),
        (
            EventKind::AssistantMessage,
            json!({"blocks": [{"type": "text", "text": "done that"}]}),
        ),
        (
            EventKind::TurnEnded,
            json!({"reason": "done", "touched": touched}),
        ),
    ]
}

/// A thread whose log holds `events`, in project `dir`, with `script`
/// answering its model calls, and the utility set when `utility` is.
fn thread(
    dir: &tempfile::TempDir,
    events: Vec<(EventKind, serde_json::Value)>,
    script: Vec<Vec<ProviderEvent>>,
    utility: Option<Vec<Vec<ProviderEvent>>>,
) -> (Runtime, Seen, Option<Seen>) {
    let id = Ulid::generate();
    let logs = dir.path().join("threads");
    std::fs::create_dir_all(&logs).unwrap();
    let mut log = ThreadLog::open(&logs, id).unwrap();
    for (kind, payload) in events {
        log.append(NewEvent {
            kind,
            author: Author::System,
            payload,
            parent_event: None,
        })
        .unwrap();
    }
    let (provider, seen) = scripted(script);
    let global = GlobalLayer::load(
        &dir.path().join("config/instructions.md"),
        None,
        None,
        Vec::new(),
        Vec::new(),
    )
    .unwrap();
    let mut runtime = Runtime::new(
        provider,
        ToolRegistry::from(Vec::<Box<dyn aigentic_core::Tool>>::new()),
        ThreadLog::open(&logs, id).unwrap(),
        aigentic_core::AgentId("worker".into()),
    )
    .with_layers(Layers {
        global,
        workspace: None,
        project: Some(Project::open(dir.path()).unwrap().unwrap()),
    })
    .with_model_label("thread-model")
    .with_pricing(
        "thread",
        Some(Prices {
            input: 1.0,
            cache_read: 0.0,
            cache_write: 0.0,
            output: 1.0,
        }),
    );
    let mut utility_seen = None;
    if let Some(script) = utility {
        let (provider, seen) = scripted(script);
        runtime = runtime.with_utility(
            provider,
            "utility-model",
            Some(Prices {
                input: 2.0,
                cache_read: 0.0,
                cache_write: 0.0,
                output: 2.0,
            }),
        );
        utility_seen = Some(seen);
    }
    (runtime, seen, utility_seen)
}

fn reply() -> Vec<Vec<ProviderEvent>> {
    vec![vec![
        ProviderEvent::TextDelta(REPLY.into()),
        usage(1_000, 100),
    ]]
}

fn sent(seen: &Seen) -> Vec<Vec<Message>> {
    seen.lock().unwrap().clone()
}

fn text_of(messages: &[Message]) -> String {
    messages
        .iter()
        .flat_map(|m| m.blocks.iter())
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn refreshes(runtime: &Runtime) -> Vec<BriefRefreshedPayload> {
    runtime
        .log()
        .read_all()
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EventKind::BriefRefreshed)
        .map(|e| serde_json::from_value(e.payload).unwrap())
        .collect()
}

#[tokio::test]
async fn a_batch_that_wrote_files_rewrites_the_current_state_once() {
    let dir = project(true, "");
    let (mut runtime, seen, _) = thread(&dir, turn("fix it", &["src/lib.rs"]), reply(), None);

    let payload = runtime
        .refresh_project_brief(&mut |_| {})
        .await
        .unwrap()
        .expect("a refresh ran");
    assert_eq!(sent(&seen).len(), 1, "one call");
    let brief = std::fs::read_to_string(project_brief_path(dir.path())).unwrap();
    assert!(brief.contains(REPLY), "{brief}");
    assert!(!brief.contains("Nothing yet."), "{brief}");
    assert!(
        brief.contains("## Notes\n\nKeep this."),
        "the rest is kept: {brief}"
    );
    assert_ne!(payload.old_hash, payload.new_hash);
    assert_eq!(payload.model, "thread-model");
    assert_eq!(refreshes(&runtime), vec![payload]);

    // Nothing new since: no second call, no second event.
    assert!(
        runtime
            .refresh_project_brief(&mut |_| {})
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(sent(&seen).len(), 1);
    assert_eq!(refreshes(&runtime).len(), 1);
}

#[tokio::test]
async fn nothing_runs_without_writes_without_a_brief_or_with_refresh_off() {
    let quiet = project(true, "");
    let (mut runtime, seen, _) = thread(&quiet, turn("look around", &[]), reply(), None);
    assert!(
        runtime
            .refresh_project_brief(&mut |_| {})
            .await
            .unwrap()
            .is_none()
    );
    assert!(sent(&seen).is_empty(), "no writes, no call");

    let briefless = project(false, "");
    let (mut runtime, seen, _) = thread(&briefless, turn("fix it", &["a.rs"]), reply(), None);
    assert!(
        runtime
            .refresh_project_brief(&mut |_| {})
            .await
            .unwrap()
            .is_none()
    );
    assert!(sent(&seen).is_empty(), "no brief, no call");
    assert!(
        !project_brief_path(briefless.path()).exists(),
        "and no brief made"
    );

    let off = project(true, "[brief]\nrefresh = false\n");
    let (mut runtime, seen, _) = thread(&off, turn("fix it", &["a.rs"]), reply(), None);
    assert!(
        runtime
            .refresh_project_brief(&mut |_| {})
            .await
            .unwrap()
            .is_none()
    );
    assert!(sent(&seen).is_empty(), "refresh off, no call");
    assert!(refreshes(&runtime).is_empty());
}

#[tokio::test]
async fn the_utility_runs_the_refresh_and_is_named_and_priced_for_it() {
    let dir = project(true, "");
    let (mut runtime, seen, utility) =
        thread(&dir, turn("fix it", &["a.rs"]), Vec::new(), Some(reply()));
    let payload = runtime
        .refresh_project_brief(&mut |_| {})
        .await
        .unwrap()
        .expect("a refresh ran");
    assert!(
        sent(&seen).is_empty(),
        "the thread's own model is not called"
    );
    assert_eq!(sent(&utility.unwrap()).len(), 1);
    assert_eq!(payload.model, "utility-model");
    // 1,000 in and 100 out at the utility's 2.0 a million each.
    assert_eq!(payload.usage.cost_usd, Some(0.0022));
}

#[tokio::test]
async fn the_record_carries_only_the_turns_after_the_last_refresh() {
    let dir = project(true, "");
    let mut events = turn(EARLY, &["a.rs"]);
    let through = events.len() as u64 - 1;
    events.push((
        EventKind::BriefRefreshed,
        json!({
            "through_seq": through,
            "old_hash": "x",
            "new_hash": "y",
            "model": "m",
            "usage": {"input_tokens": 0, "output_tokens": 0},
        }),
    ));
    events.extend(turn(LATE, &["b.rs"]));
    let (mut runtime, seen, _) = thread(&dir, events, reply(), None);
    runtime
        .refresh_project_brief(&mut |_| {})
        .await
        .unwrap()
        .expect("a refresh ran");
    let asked = text_of(&sent(&seen)[0]);
    assert!(asked.contains(LATE), "{asked}");
    assert!(!asked.contains(EARLY), "{asked}");
    assert!(asked.contains("b.rs"), "{asked}");
}
