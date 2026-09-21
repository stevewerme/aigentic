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
use aigentic_runtime::{KnowledgeMode, Layers, Project, Runtime};
use aigentic_tools::ToolRegistry;
use common::{Seen, done};
use futures_core::Stream;
use serde_json::json;

/// Counts a token per four characters; window of 1000 tokens, so a
/// threshold of 0.4 is 400 tokens or 1600 characters.
struct Counting {
    script: Mutex<std::collections::VecDeque<Vec<ProviderEvent>>>,
    seen: Seen,
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
            max_context_tokens: 1000,
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
