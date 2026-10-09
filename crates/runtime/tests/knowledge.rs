//! Knowledge in the loop: inline under the threshold, index and
//! `search_knowledge` over it, re-decided when the folder changes.

mod common;

use std::pin::Pin;
use std::sync::{Arc, Mutex};

use aigentic_core::{
    Author, Capabilities, CompletionRequest, ContentBlock, Message, Provider, ProviderEvent, Role,
    ToolCall, UserId,
};
use aigentic_log::{ThreadLog, ToolResultPayload};
use aigentic_runtime::{KnowledgeMode, Layers, Project, ProjectRow, Runtime, WorkspaceLayer};
use aigentic_tools::ToolRegistry;
use common::{Seen, done};
use futures_core::Stream;
use serde_json::json;

/// Counts a token per four characters; window of 1000 tokens, so a
/// threshold of 0.4 is 400 tokens or 1600 characters.
struct Counting {
    script: Mutex<std::collections::VecDeque<Vec<ProviderEvent>>>,
    seen: Seen,
    window: u64,
}

impl Provider for Counting {
    fn complete(
        &self,
        request: &CompletionRequest<'_>,
    ) -> Pin<Box<dyn Stream<Item = ProviderEvent> + Send + '_>> {
        self.seen.lock().unwrap().push(request.messages.to_vec());
        let events = self.script.lock().unwrap().pop_front().expect("script");
        Box::pin(futures_util::stream::iter(events))
    }
    fn count_tokens(&self, messages: &[Message]) -> u64 {
        messages
            .iter()
            .flat_map(|m| &m.blocks)
            .map(|b| match b {
                ContentBlock::Text(t) => t.len() as u64 / 4,
                _ => 1,
            })
            .sum()
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_tools: true,
            supports_images: false,
            supports_caching: false,
            supports_structured_output: false,
            max_context_tokens: self.window,
        }
    }
}

fn steve() -> Author {
    Author::User(UserId("steve".into()))
}

fn project_dir(knowledge_bytes: usize) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("aigentic.toml"),
        "[project]\nname = \"k\"\n",
    )
    .unwrap();
    let kd = dir.path().join(".aigentic/knowledge");
    std::fs::create_dir_all(&kd).unwrap();
    std::fs::write(
        kd.join("ops.md"),
        "# Deploys\n\nWe deploy on Fridays.\n\n## Rollback\n\nRun vercel rollback.\n",
    )
    .unwrap();
    if knowledge_bytes > 0 {
        let filler = "# Filler\n\n".to_owned() + &"lorem ipsum ".repeat(knowledge_bytes / 12);
        std::fs::write(kd.join("big.md"), filler).unwrap();
    }
    dir
}

fn rig(dir: &tempfile::TempDir, script: Vec<Vec<ProviderEvent>>) -> (Runtime, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let provider = Counting {
        script: Mutex::new(script.into()),
        seen: seen.clone(),
        window: 1000,
    };
    let log = ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap();
    let project = Project::open(dir.path()).unwrap().unwrap();
    let runtime = Runtime::new(
        Box::new(provider),
        ToolRegistry::empty(),
        log,
        aigentic_core::AgentId("worker".into()),
    )
    .with_layers(Layers::default().with_project(project));
    (runtime, seen)
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

fn text(t: &str) -> ProviderEvent {
    ProviderEvent::TextDelta(t.into())
}

#[tokio::test]
async fn a_small_folder_is_inlined_and_no_search_tool_is_offered() {
    let dir = project_dir(0);
    let (mut rt, seen) = rig(&dir, vec![vec![text("ok"), done("stop")]]);
    assert_eq!(rt.knowledge_mode(), KnowledgeMode::Inline);
    let names: Vec<String> = rt.tool_specs().into_iter().map(|s| s.name).collect();
    assert!(!names.contains(&"search_knowledge".to_owned()), "{names:?}");
    rt.run_turn(steve(), vec![ContentBlock::Text("hi".into())], &mut |_| {})
        .await
        .unwrap();
    let ctx = &seen.lock().unwrap()[0];
    assert_eq!(ctx[0].role, Role::System);
    let block = texts(&ctx[0]);
    assert!(
        block.starts_with("# Project knowledge\n\n## ops.md\n\n# Deploys"),
        "{block}"
    );
    assert!(block.contains("vercel rollback"));
}

#[tokio::test]
async fn a_large_folder_is_indexed_and_search_knowledge_finds_a_section() {
    let dir = project_dir(4000);
    let (mut rt, seen) = rig(
        &dir,
        vec![
            vec![
                ProviderEvent::ToolCall(ToolCall {
                    id: "c1".into(),
                    name: "search_knowledge".into(),
                    args: json!({"query": "how do we rollback"}),
                }),
                done("tool_use"),
            ],
            vec![text("run vercel rollback"), done("stop")],
        ],
    );
    assert_eq!(rt.knowledge_mode(), KnowledgeMode::Index);
    assert!(rt.knowledge().tokens > 400);
    let names: Vec<String> = rt.tool_specs().into_iter().map(|s| s.name).collect();
    assert!(names.contains(&"search_knowledge".to_owned()), "{names:?}");
    rt.run_turn(steve(), vec![ContentBlock::Text("hi".into())], &mut |_| {})
        .await
        .unwrap();
    let ctx = &seen.lock().unwrap()[0];
    let block = texts(&ctx[0]);
    assert!(block.starts_with("# Project knowledge (index)"), "{block}");
    assert!(block.contains("- big.md: Filler (1 sections)"), "{block}");
    assert!(block.contains("- ops.md: Deploys (2 sections)"), "{block}");
    assert!(!block.contains("lorem"), "the index never inlines the text");
    let events = rt.log().read_all().unwrap();
    let r: ToolResultPayload = serde_json::from_value(events[2].payload.clone()).unwrap();
    assert!(!r.result.is_error);
    assert!(
        r.result.content.starts_with("ops.md#Rollback\n## Rollback"),
        "{}",
        r.result.content
    );
    assert_eq!(
        r.policy,
        Some(aigentic_log::PolicyRecord::rule("class read", "allow"))
    );
}

#[tokio::test]
async fn a_change_on_disk_is_picked_up_at_the_next_turn_and_can_flip_the_mode() {
    let dir = project_dir(0);
    let (mut rt, seen) = rig(
        &dir,
        vec![
            vec![text("one"), done("stop")],
            vec![text("two"), done("stop")],
        ],
    );
    rt.run_turn(steve(), vec![ContentBlock::Text("a".into())], &mut |_| {})
        .await
        .unwrap();
    assert_eq!(rt.knowledge_mode(), KnowledgeMode::Inline);
    let filler = "# Filler\n\n".to_owned() + &"lorem ipsum ".repeat(400);
    std::fs::write(dir.path().join(".aigentic/knowledge/big.md"), filler).unwrap();
    rt.run_turn(steve(), vec![ContentBlock::Text("b".into())], &mut |_| {})
        .await
        .unwrap();
    assert_eq!(
        rt.knowledge_mode(),
        KnowledgeMode::Index,
        "re-decided at the turn boundary"
    );
    let names: Vec<String> = rt.tool_specs().into_iter().map(|s| s.name).collect();
    assert!(names.contains(&"search_knowledge".to_owned()));
    let seen = seen.lock().unwrap();
    assert!(texts(&seen[0][0]).starts_with("# Project knowledge\n"));
    assert!(texts(&seen[1][0]).starts_with("# Project knowledge (index)"));
}

#[tokio::test]
async fn set_provider_re_decides_the_mode_for_the_new_window() {
    // ~200 tokens of knowledge: inline at a 1000 window (line 400),
    // index at a 100 window (line 40).
    let dir = project_dir(800);
    let (mut rt, _seen) = rig(&dir, vec![]);
    assert_eq!(rt.knowledge_mode(), KnowledgeMode::Inline);
    let has_search = |rt: &Runtime| {
        rt.tool_specs()
            .into_iter()
            .any(|s| s.name == "search_knowledge")
    };
    assert!(!has_search(&rt));
    let small = Counting {
        script: Mutex::new(Vec::new().into()),
        seen: Arc::new(Mutex::new(Vec::new())),
        window: 100,
    };
    rt.set_provider(Box::new(small), "small", None, None)
        .unwrap();
    assert_eq!(rt.knowledge_mode(), KnowledgeMode::Index);
    assert!(has_search(&rt), "search_knowledge registered on the swap");
    assert_eq!(rt.model_label(), "small");
    let big = Counting {
        script: Mutex::new(Vec::new().into()),
        seen: Arc::new(Mutex::new(Vec::new())),
        window: 1000,
    };
    rt.set_provider(Box::new(big), "big", None, None).unwrap();
    assert_eq!(rt.knowledge_mode(), KnowledgeMode::Inline);
    assert!(!has_search(&rt), "and removed on the swap back");
}

// ---- Siblings, the workspace and the boundary ----

/// A project folder named `name`, with the knowledge and memory files a
/// test asks for (a relative path under the folder).
fn tree(name: &str, files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("aigentic.toml"),
        format!("[project]\nname = \"{name}\"\n"),
    )
    .unwrap();
    for (rel, text) in files {
        let path = dir.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    dir
}

fn row(
    name: &str,
    root: &std::path::Path,
    workspace: Option<&str>,
    understood: bool,
) -> ProjectRow {
    ProjectRow {
        name: name.to_owned(),
        root: root.to_path_buf(),
        workspace: workspace.map(str::to_owned),
        one_line: Some(format!("{name} does its thing")),
        understood,
    }
}

/// A runtime over `own`, with the rows and workspace layer a test hands
/// in: the shape the daemon builds.
fn scope_rig(
    own: &std::path::Path,
    rows: Vec<ProjectRow>,
    listed: &str,
    workspace: Option<WorkspaceLayer>,
    script: Vec<Vec<ProviderEvent>>,
) -> (Runtime, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let provider = Counting {
        script: Mutex::new(script.into()),
        seen: seen.clone(),
        window: 1000,
    };
    let log = ThreadLog::open(own, ulid::Ulid::generate()).unwrap();
    let project = Project::open(own).unwrap().unwrap();
    let layers = Layers {
        workspace,
        ..Layers::default().with_project(project)
    };
    let runtime = Runtime::new(
        Box::new(provider),
        ToolRegistry::empty(),
        log,
        aigentic_core::AgentId("worker".into()),
    )
    .with_layers(layers)
    .with_projects(Some(listed.to_owned()), rows);
    (runtime, seen)
}

/// Every tool result in the log, in order.
fn results(rt: &Runtime) -> Vec<ToolResultPayload> {
    rt.log()
        .read_all()
        .unwrap()
        .into_iter()
        .filter_map(|e| serde_json::from_value::<ToolResultPayload>(e.payload).ok())
        .collect()
}

fn call(name: &str, args: serde_json::Value) -> ProviderEvent {
    ProviderEvent::ToolCall(ToolCall {
        id: format!("c-{name}"),
        name: name.into(),
        args,
    })
}

/// One turn that runs `calls` and then answers.
fn calls_turn(calls: Vec<(&str, serde_json::Value)>) -> Vec<Vec<ProviderEvent>> {
    let mut step: Vec<ProviderEvent> = calls.into_iter().map(|(n, a)| call(n, a)).collect();
    step.push(done("tool_use"));
    vec![step, vec![text("ok"), done("stop")]]
}

fn has_search(rt: &Runtime) -> bool {
    rt.tool_specs()
        .into_iter()
        .any(|s| s.name == "search_knowledge")
}

/// The thread's own project, named `p` so it matches the rows a test
/// hands in.
const OWN: &[(&str, &str)] = &[(
    ".aigentic/knowledge/ops.md",
    "# Deploys\n\nWe deploy on Fridays.\n\n## Rollback\n\nRun vercel rollback.\n",
)];

/// The system text of one request, its blocks joined.
fn system_text(request: &[Message]) -> String {
    request
        .iter()
        .filter(|m| m.role == Role::System)
        .flat_map(|m| m.blocks.iter())
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Write `rel` under `root`, making the folders on the way.
fn write_file(root: &std::path::Path, rel: &str, text: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A filler file big enough to push a folder over the inline threshold.
fn filler_file(dir: &std::path::Path) {
    let path = dir.join(".aigentic/knowledge/big.md");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        "# Filler\n\n".to_owned() + &"lorem ipsum ".repeat(400),
    )
    .unwrap();
}

const SIBLING: &[(&str, &str)] = &[
    (
        ".aigentic/knowledge/deploy.md",
        "# Deploys\n\nWe deploy on Fridays.\n",
    ),
    (
        ".aigentic/memory/decisions.md",
        "# Storage\n\nWe chose Postgres for storage.\n",
    ),
    (
        ".aigentic/brief.md",
        "# Foundation\n\nThe sibling's own brief.\n",
    ),
];

#[tokio::test]
async fn a_sibling_in_the_workspace_is_searchable_and_briefable() {
    let own = tree("p", OWN);
    let q = tree("q", SIBLING);
    let rows = vec![
        row("p", own.path(), Some("w"), true),
        row("q", q.path(), Some("w"), true),
    ];
    let (mut rt, _seen) = scope_rig(
        own.path(),
        rows,
        "current project: p\n",
        None,
        calls_turn(vec![
            ("read_brief", json!({"project": "q"})),
            (
                "search_knowledge",
                json!({"query": "postgres storage", "project": "q"}),
            ),
        ]),
    );
    assert!(has_search(&rt), "a sibling with knowledge offers the tool");
    rt.run_turn(steve(), vec![ContentBlock::Text("hi".into())], &mut |_| {})
        .await
        .unwrap();
    let results = results(&rt);
    assert_eq!(results.len(), 2);
    assert_eq!(
        results[0].result.content,
        "[read-only · q]\n# Foundation\n\nThe sibling's own brief.\n"
    );
    assert!(
        results[1]
            .result
            .content
            .starts_with("[read-only · q]\nmemory/decisions.md#Storage\n"),
        "{}",
        results[1].result.content
    );
}

#[tokio::test]
async fn a_project_in_another_workspace_is_refused_by_both_tools() {
    let own = tree("p", OWN);
    let q = tree("q", SIBLING);
    let s = tree(
        "s",
        &[(".aigentic/knowledge/notes.md", "# Notes\n\nElsewhere.\n")],
    );
    let rows = vec![
        row("p", own.path(), Some("w"), true),
        row("q", q.path(), Some("w"), true),
        row("s", s.path(), Some("other"), false),
    ];
    let listed = "current project: p\nworkspace w: q\nother: s (elsewhere)\n";
    let (mut rt, seen) = scope_rig(
        own.path(),
        rows,
        listed,
        None,
        calls_turn(vec![
            ("read_brief", json!({"project": "s"})),
            (
                "search_knowledge",
                json!({"query": "elsewhere", "project": "s"}),
            ),
        ]),
    );
    rt.run_turn(steve(), vec![ContentBlock::Text("hi".into())], &mut |_| {})
        .await
        .unwrap();
    let results = results(&rt);
    assert_eq!(results.len(), 2);
    for r in &results {
        assert!(r.result.is_error, "{}", r.result.content);
        // Neither refusal offers `s` among the projects that would work.
        assert!(
            r.result
                .content
                .ends_with("the projects with a brief are: p, q")
                || r.result.content.ends_with("searchable: p, q"),
            "{}",
            r.result.content
        );
    }
    assert!(
        results[1]
            .result
            .content
            .contains("`s` is not a project this thread understands"),
        "{}",
        results[1].result.content
    );
    // The block the thread is shown is the one the daemon rendered from
    // every row: the tightening is in what the tools accept, not in what
    // the thread is told exists.
    let block = texts(&seen.lock().unwrap()[0][0]);
    assert!(block.ends_with(listed), "{block}");
}

#[tokio::test]
async fn a_thread_in_no_workspace_understands_only_itself() {
    let own = tree("p", OWN);
    filler_file(own.path());
    let q = tree("q", SIBLING);
    let rows = vec![
        row("p", own.path(), None, true),
        row("q", q.path(), Some("w"), false),
    ];
    let (mut rt, _seen) = scope_rig(
        own.path(),
        rows,
        "current project: p\n",
        None,
        calls_turn(vec![
            ("read_brief", json!({"project": "q"})),
            (
                "search_knowledge",
                json!({"query": "fridays", "project": "q"}),
            ),
        ]),
    );
    rt.run_turn(steve(), vec![ContentBlock::Text("hi".into())], &mut |_| {})
        .await
        .unwrap();
    let results = results(&rt);
    for r in &results {
        assert!(r.result.is_error, "{}", r.result.content);
    }
    assert_eq!(
        results[0].result.content, "no project in reach has a brief",
        "the sibling in another workspace is no reason to offer it"
    );
    assert!(
        results[1].result.content.ends_with("searchable: p"),
        "only its own project is searchable: {}",
        results[1].result.content
    );
}

#[tokio::test]
async fn the_threads_own_project_is_searchable_by_name_read_only() {
    let own = tree("p", OWN);
    filler_file(own.path());
    let rows = vec![row("p", own.path(), None, true)];
    let (mut rt, _seen) = scope_rig(
        own.path(),
        rows,
        "current project: p\n",
        None,
        calls_turn(vec![(
            "search_knowledge",
            json!({"query": "rollback", "project": "p"}),
        )]),
    );
    rt.run_turn(steve(), vec![ContentBlock::Text("hi".into())], &mut |_| {})
        .await
        .unwrap();
    let results = results(&rt);
    assert!(
        results[0]
            .result
            .content
            .starts_with("[read-only · p]\nops.md#Rollback"),
        "{}",
        results[0].result.content
    );
}

#[tokio::test]
async fn a_sibling_edit_is_seen_next_call_and_an_unchanged_one_is_not_re_read() {
    let own = tree("p", OWN);
    let q = tree(
        "q",
        &[(
            ".aigentic/knowledge/deploy.md",
            "# Deploys\n\nWe deploy on Fridays.\n",
        )],
    );
    let rows = vec![
        row("p", own.path(), Some("w"), true),
        row("q", q.path(), Some("w"), true),
    ];
    let ask = || {
        calls_turn(vec![(
            "search_knowledge",
            json!({"query": "deploys", "project": "q"}),
        )])
    };
    let script: Vec<Vec<ProviderEvent>> = [ask(), ask(), ask(), ask()].concat();
    let (mut rt, _seen) = scope_rig(own.path(), rows, "current project: p\n", None, script);
    for _ in 0..3 {
        rt.run_turn(steve(), vec![ContentBlock::Text("hi".into())], &mut |_| {})
            .await
            .unwrap();
    }
    let loads = rt.scope_sources().loads();
    let found = results(&rt);
    assert_eq!(loads, 1, "read once, then served from the cache");
    assert!(
        found[0].result.content.ends_with("We deploy on Fridays."),
        "{}",
        found[0].result.content
    );

    std::fs::write(
        q.path().join(".aigentic/knowledge/deploy.md"),
        "# Deploys\n\nWe deploy on Tuesdays now.\n",
    )
    .unwrap();
    rt.run_turn(steve(), vec![ContentBlock::Text("hi".into())], &mut |_| {})
        .await
        .unwrap();
    let found = results(&rt);
    assert_eq!(
        rt.scope_sources().loads(),
        loads + 1,
        "re-read after the edit"
    );
    assert!(
        found[3]
            .result
            .content
            .ends_with("We deploy on Tuesdays now."),
        "{}",
        found[3].result.content
    );
}

#[tokio::test]
async fn a_sibling_file_that_leaves_the_sibling_is_skipped_and_noted() {
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret.md");
    std::fs::write(&secret, "# Secret\n\nAnother project's deploys.\n").unwrap();
    let own = tree("p", OWN);
    let q = tree(
        "q",
        &[(
            ".aigentic/knowledge/deploy.md",
            "# Deploys\n\nWe deploy on Fridays.\n",
        )],
    );
    std::os::unix::fs::symlink(&secret, q.path().join(".aigentic/knowledge/link.md")).unwrap();
    let rows = vec![
        row("p", own.path(), Some("w"), true),
        row("q", q.path(), Some("w"), true),
    ];
    let (mut rt, _seen) = scope_rig(
        own.path(),
        rows,
        "current project: p\n",
        None,
        calls_turn(vec![(
            "search_knowledge",
            json!({"query": "secret deploys", "project": "q"}),
        )]),
    );
    rt.run_turn(steve(), vec![ContentBlock::Text("hi".into())], &mut |_| {})
        .await
        .unwrap();
    let content = results(&rt)[0].result.content.clone();
    assert!(
        content.starts_with("[read-only · q]\nskipped 1 files outside q\n"),
        "{content}"
    );
    assert!(!content.contains("Another project"), "{content}");
    assert!(content.contains("We deploy on Fridays."), "{content}");
}

#[tokio::test]
async fn the_search_tool_is_offered_for_a_sibling_or_the_workspace_that_has_anything() {
    let own = tree("p", OWN);
    let empty = tree("q", &[]);
    let rows = |q: &std::path::Path| {
        vec![
            row("p", own.path(), Some("w"), true),
            row("q", q, Some("w"), true),
        ]
    };
    let bare = |workspace: Option<WorkspaceLayer>| {
        scope_rig(
            own.path(),
            rows(empty.path()),
            "current project: p\n",
            workspace,
            Vec::new(),
        )
        .0
    };

    // Inline project knowledge is not indexed, and nothing else is
    // searchable, so the tool is not offered.
    assert!(!has_search(&bare(None)));

    // A sibling with memory alone is enough.
    let memory = tree(
        "q",
        &[(".aigentic/memory/facts.md", "# Facts\n\nPorts are 8080.\n")],
    );
    let rt = scope_rig(
        own.path(),
        rows(memory.path()),
        "current project: p\n",
        None,
        Vec::new(),
    )
    .0;
    assert!(has_search(&rt), "memory counts");

    // So is a sibling with knowledge.
    let sibling = tree(
        "q",
        &[(
            ".aigentic/knowledge/ops.md",
            "# Ops\n\nDeploys run at noon.\n",
        )],
    );
    let rt = scope_rig(
        own.path(),
        rows(sibling.path()),
        "current project: p\n",
        None,
        Vec::new(),
    )
    .0;
    assert!(has_search(&rt), "sibling knowledge counts");

    // A sibling the thread does not understand does not.
    let mut outside = rows(empty.path());
    outside[1].understood = false;
    let rt = scope_rig(
        own.path(),
        outside,
        "current project: p\n",
        None,
        Vec::new(),
    )
    .0;
    assert!(
        !has_search(&rt),
        "an out-of-workspace sibling is not understood"
    );

    // So is a workspace with knowledge.
    let shared = tempfile::tempdir().unwrap();
    write_file(
        shared.path(),
        "workspace/knowledge/shared.md",
        "# Shared\n\nThe workspace deploys too.\n",
    );
    let layer = WorkspaceLayer::load("w", Some(shared.path())).unwrap();
    let rt = scope_rig(
        own.path(),
        rows(empty.path()),
        "current project: p\n",
        Some(layer),
        Vec::new(),
    )
    .0;
    assert!(has_search(&rt), "the workspace's knowledge counts");
}

#[tokio::test]
async fn the_search_tool_is_offered_for_an_indexed_project_and_names_both_arguments() {
    let own = tree("p", OWN);
    filler_file(own.path());
    let (rt, _seen) = scope_rig(
        own.path(),
        vec![row("p", own.path(), Some("w"), true)],
        "current project: p\n",
        None,
        Vec::new(),
    );
    assert_eq!(rt.knowledge_mode(), KnowledgeMode::Index);
    let spec = rt
        .tool_specs()
        .into_iter()
        .find(|s| s.name == "search_knowledge")
        .expect("offered");
    for argument in ["project", "workspace"] {
        assert!(spec.description.contains(argument), "{}", spec.description);
    }
    let schema = serde_json::to_string(&spec.schema).unwrap();
    assert!(schema.contains("\"project\""), "{schema}");
    assert!(schema.contains("\"workspace\""), "{schema}");
}

#[tokio::test]
async fn a_workspace_layer_without_knowledge_leaves_the_context_alone() {
    let own = tree("p", OWN);
    let (mut plain, seen) = scope_rig(
        own.path(),
        vec![row("p", own.path(), Some("w"), true)],
        "current project: p\n",
        None,
        vec![vec![text("ok"), done("stop")]],
    );
    plain
        .run_turn(steve(), vec![ContentBlock::Text("hi".into())], &mut |_| {})
        .await
        .unwrap();
    let without = seen.lock().unwrap()[0].clone();

    let layer = WorkspaceLayer::load("w", None).unwrap();
    let (mut with, seen) = scope_rig(
        own.path(),
        vec![row("p", own.path(), Some("w"), true)],
        "current project: p\n",
        Some(layer),
        vec![vec![text("ok"), done("stop")]],
    );
    with.run_turn(steve(), vec![ContentBlock::Text("hi".into())], &mut |_| {})
        .await
        .unwrap();
    let present = seen.lock().unwrap()[0].clone();
    assert_eq!(
        without.iter().map(texts).collect::<Vec<_>>(),
        present.iter().map(texts).collect::<Vec<_>>(),
        "a workspace with no knowledge adds no block"
    );
}

#[tokio::test]
async fn a_workspace_small_knowledge_is_inlined_before_the_project_knowledge() {
    let own = tree("p", OWN);
    let shared = tempfile::tempdir().unwrap();
    write_file(
        shared.path(),
        "workspace/knowledge/shared.md",
        "# Shared\n\nEveryone deploys on Fridays.\n",
    );
    let layer = WorkspaceLayer::load("w", Some(shared.path())).unwrap();
    let (mut rt, seen) = scope_rig(
        own.path(),
        vec![row("p", own.path(), Some("w"), true)],
        "current project: p\n",
        Some(layer),
        vec![vec![text("ok"), done("stop")]],
    );
    rt.run_turn(steve(), vec![ContentBlock::Text("hi".into())], &mut |_| {})
        .await
        .unwrap();
    let seen = seen.lock().unwrap();
    let blocks: Vec<String> = seen[0]
        .iter()
        .flat_map(|m| m.blocks.iter())
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.clone()),
            _ => None,
        })
        .collect();
    let at = |needle: &str| blocks.iter().position(|b| b.starts_with(needle));
    let workspace = at("# Workspace knowledge: w").expect("the workspace block");
    let project = at("# Project knowledge").expect("the project block");
    assert_eq!(workspace + 1, project, "it sits right before the project's");
    assert!(blocks[workspace].contains("Everyone deploys on Fridays."));
    assert!(!blocks[workspace].contains("(index)"));
}

#[tokio::test]
async fn a_large_workspace_knowledge_is_indexed_and_searchable_by_workspace() {
    let own = tree("p", OWN);
    let shared = tempfile::tempdir().unwrap();
    write_file(
        shared.path(),
        "workspace/knowledge/big.md",
        &("# Shared\n\n".to_owned() + &"lorem ipsum ".repeat(200)),
    );
    write_file(
        shared.path(),
        "workspace/memory/decisions.md",
        "# Choice\n\nWe chose the shared log.\n",
    );
    let layer = WorkspaceLayer::load("w", Some(shared.path())).unwrap();
    let (mut rt, seen) = scope_rig(
        own.path(),
        vec![row("p", own.path(), Some("w"), true)],
        "current project: p\n",
        Some(layer),
        calls_turn(vec![(
            "search_knowledge",
            json!({"query": "lorem ipsum", "workspace": true}),
        )]),
    );
    rt.run_turn(steve(), vec![ContentBlock::Text("hi".into())], &mut |_| {})
        .await
        .unwrap();
    let request = seen.lock().unwrap()[0].clone();
    let block = system_text(&request);
    assert!(
        block.contains("# Workspace knowledge: w (index)"),
        "{block}"
    );
    assert!(block.contains("- big.md: Shared (1 sections)"), "{block}");
    assert!(block.contains("workspace: true"), "{block}");
    let results = results(&rt);
    assert!(
        results[0]
            .result
            .content
            .starts_with("[read-only · workspace w]\nbig.md#Shared"),
        "{}",
        results[0].result.content
    );
    assert!(!results[0].result.is_error);
}

#[tokio::test]
async fn a_project_that_inlines_leaves_the_workspace_the_rest_of_the_budget() {
    // ~200 tokens of project knowledge: inline under a 400-token line.
    let own = tree("p", OWN);
    write_file(
        own.path(),
        ".aigentic/knowledge/filler.md",
        &("# Filler\n\n".to_owned() + &"lorem ipsum ".repeat(66)),
    );

    // ~130 tokens of workspace knowledge fits in what is left.
    let shared = tempfile::tempdir().unwrap();
    write_file(
        shared.path(),
        "workspace/knowledge/small.md",
        &("# Small\n\n".to_owned() + &"lorem ipsum ".repeat(40)),
    );
    let layer = WorkspaceLayer::load("w", Some(shared.path())).unwrap();
    let (mut rt, seen) = scope_rig(
        own.path(),
        vec![row("p", own.path(), Some("w"), true)],
        "current project: p\n",
        Some(layer),
        vec![vec![text("ok"), done("stop")]],
    );
    assert_eq!(rt.knowledge_mode(), KnowledgeMode::Inline);
    rt.run_turn(steve(), vec![ContentBlock::Text("hi".into())], &mut |_| {})
        .await
        .unwrap();
    let request = seen.lock().unwrap()[0].clone();
    let block = system_text(&request);
    assert!(block.contains("# Workspace knowledge: w\n"), "{block}");

    // ~420 tokens does not, so the workspace is indexed and the project
    // still inlines.
    let shared = tempfile::tempdir().unwrap();
    write_file(
        shared.path(),
        "workspace/knowledge/big.md",
        &("# Big\n\n".to_owned() + &"lorem ipsum ".repeat(130)),
    );
    let layer = WorkspaceLayer::load("w", Some(shared.path())).unwrap();
    let (mut rt, seen) = scope_rig(
        own.path(),
        vec![row("p", own.path(), Some("w"), true)],
        "current project: p\n",
        Some(layer),
        vec![vec![text("ok"), done("stop")]],
    );
    assert_eq!(rt.knowledge_mode(), KnowledgeMode::Inline);
    rt.run_turn(steve(), vec![ContentBlock::Text("hi".into())], &mut |_| {})
        .await
        .unwrap();
    let request = seen.lock().unwrap()[0].clone();
    let block = system_text(&request);
    assert!(
        block.contains("# Workspace knowledge: w (index)"),
        "{block}"
    );
    assert!(block.contains("# Project knowledge\n"), "{block}");
}

#[tokio::test]
async fn a_workspace_edit_is_seen_at_the_next_turn() {
    let own = tree("p", OWN);
    let shared = tempfile::tempdir().unwrap();
    write_file(
        shared.path(),
        "workspace/knowledge/shared.md",
        "# Shared\n\nWe deploy on Fridays.\n",
    );
    let (mut rt, seen) = scope_rig(
        own.path(),
        vec![row("p", own.path(), Some("w"), true)],
        "current project: p\n",
        Some(WorkspaceLayer::load("w", Some(shared.path())).unwrap()),
        vec![
            vec![text("one"), done("stop")],
            vec![text("two"), done("stop")],
        ],
    );
    rt.run_turn(steve(), vec![ContentBlock::Text("a".into())], &mut |_| {})
        .await
        .unwrap();
    assert!(system_text(&seen.lock().unwrap()[0]).contains("We deploy on Fridays."));
    write_file(
        shared.path(),
        "workspace/knowledge/shared.md",
        "# Shared\n\nWe deploy on Tuesdays now.\n",
    );
    rt.run_turn(steve(), vec![ContentBlock::Text("b".into())], &mut |_| {})
        .await
        .unwrap();
    let block = system_text(&seen.lock().unwrap()[1]);
    assert!(block.contains("We deploy on Tuesdays now."), "{block}");
}
