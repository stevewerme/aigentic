//! Issue #30: a long turn's context stays bounded. A build is one turn
//! of hundreds of calls; nothing else shrinks it, so the sweep stubs, in
//! projection, the results and successful edit arguments older than the
//! last few calls.

use std::pin::Pin;
use std::sync::{Arc, Mutex};

use aigentic_core::{
    AgentId, Author, Capabilities, CompletionRequest, ContentBlock, EventKind, Message, Provider,
    ProviderEvent, Role, ToolCall, Usage,
};
use aigentic_log::ThreadLog;
use aigentic_runtime::{Answer, Approver, DEFAULT_COMPACTION, Runtime};
use futures_core::Stream;

/// Every request's messages.
type Seen = Arc<Mutex<Vec<Vec<Message>>>>;

/// Keeps the turn going: one tool call per reply for `calls` iterations
/// (a read, a write and an edit per three), then a bare text reply.
/// Reports usage from the same estimate the runtime uses.
struct Building {
    seen: Seen,
    iteration: Mutex<u32>,
    calls: u32,
}

fn estimate(context: &[Message]) -> u64 {
    aigentic_providers::estimate::estimate_tokens(context)
}

impl Provider for Building {
    fn complete(
        &self,
        request: &CompletionRequest<'_>,
    ) -> Pin<Box<dyn Stream<Item = ProviderEvent> + Send + '_>> {
        self.seen.lock().unwrap().push(request.messages.to_vec());
        let prompt = estimate(request.messages);
        let mut iteration = self.iteration.lock().unwrap();
        *iteration += 1;
        let events = if *iteration > self.calls {
            vec![
                ProviderEvent::TextDelta("done building".into()),
                ProviderEvent::Usage(Usage {
                    input_tokens: prompt,
                    output_tokens: 10,
                    ..Default::default()
                }),
                ProviderEvent::Done {
                    finish_reason: "stop".into(),
                },
            ]
        } else {
            let i = *iteration;
            let (name, args) = match i % 3 {
                1 => ("read_file", serde_json::json!({"path": "big.txt"})),
                2 => (
                    "write_file",
                    serde_json::json!({
                        "path": format!("out/{i}.txt"),
                        "content": format!("file {i} line 0\n{}", "filler line\n".repeat(400)),
                    }),
                ),
                _ => (
                    "edit_file",
                    serde_json::json!({
                        "path": format!("out/{}.txt", i - 1),
                        "old_string": format!("file {} line 0", i - 1),
                        "new_string": format!("file {} line 0 edited\nmore context", i - 1),
                    }),
                ),
            };
            vec![
                ProviderEvent::ToolCall(ToolCall {
                    id: format!("call_{i}"),
                    name: name.into(),
                    args,
                }),
                ProviderEvent::Usage(Usage {
                    input_tokens: prompt,
                    output_tokens: 20,
                    ..Default::default()
                }),
                ProviderEvent::Done {
                    finish_reason: "tool_calls".into(),
                },
            ]
        };
        Box::pin(futures_util::stream::iter(events))
    }
    fn count_tokens(&self, context: &[Message]) -> u64 {
        estimate(context)
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_tools: true,
            supports_images: false,
            supports_caching: false,
            supports_structured_output: false,
            // TensorX-shaped: a 1M window, so trigger_fraction compaction
            // would sit at 700k and never run. The sweep is all there is.
            max_context_tokens: 1_048_576,
        }
    }
}

/// Allows every prompt: the tools are real and write inside a tempdir.
struct Yes;

impl Approver for Yes {
    fn author(&self) -> Author {
        Author::User(aigentic_core::UserId("steve".into()))
    }
    fn ask(&mut self, _: &aigentic_log::PermissionRequestedPayload) -> Answer {
        Answer::Allow
    }
    fn ask_human(&mut self, _: &str) -> Option<String> {
        None
    }
}

#[tokio::test]
async fn a_two_hundred_call_turn_with_large_results_and_edits_stays_under_128k() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("out")).unwrap();
    std::fs::write(dir.path().join("big.txt"), "seed line\n".repeat(3_000)).unwrap();
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let provider = Building {
        seen: seen.clone(),
        iteration: Mutex::new(0),
        calls: 200,
    };
    let registry = aigentic_tools::ToolRegistry::builtin(aigentic_tools::Workdir::new(dir.path()));
    let log = ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap();
    let mut rt = Runtime::new(Box::new(provider), registry, log, AgentId("worker".into()))
        .with_approver(Box::new(Yes))
        .with_compaction(DEFAULT_COMPACTION)
        .with_budget(aigentic_core::Budget {
            max_iterations: 220,
            max_tokens: u64::MAX,
            max_wall_time: std::time::Duration::from_secs(300),
        })
        .with_model_label("scripted");

    let outcome = rt
        .run_turn(
            Author::User(aigentic_core::UserId("steve".into())),
            vec![ContentBlock::Text("build it".into())],
            &mut |_| {},
        )
        .await
        .unwrap();
    assert_eq!(outcome.reason, "done");
    assert_eq!(outcome.iterations, 201);

    let events = rt.log().read_all().unwrap();
    let sweeps = events
        .iter()
        .filter(|e| e.kind == EventKind::ContextEvicted)
        .count();
    assert!(
        (20..30).contains(&sweeps),
        "one sweep per block of 8 calls past the last 12, got {sweeps}"
    );

    // Every call stayed under the ceiling of tokens we are willing to
    // pay for, and the stubs did the shrinking.
    {
        let requests = seen.lock().unwrap();
        assert!(requests.len() >= 200, "{}", requests.len());
        for (i, messages) in requests.iter().enumerate() {
            let size = estimate(messages);
            assert!(size < 128_000, "request {i} was {size} tokens");
        }
        let last = requests.last().unwrap();
        let flat: String = last
            .iter()
            .flat_map(|m| m.blocks.iter())
            .map(|b| match b {
                ContentBlock::Text(t) => t.clone(),
                ContentBlock::ToolResult(r) => r.content.clone(),
                ContentBlock::ToolCall(c) => c.args.to_string(),
                _ => String::new(),
            })
            .collect();
        // Results stub to one line each; 200 calls leave at most the
        // last 12 plus the last of each distinct tool in full.
        let stubs = flat.matches("dropped from context; re-run it").count();
        assert!((150..=200).contains(&stubs), "{stubs} stubs");
        let edits = flat.matches("lines]").count();
        assert!((80..=200).contains(&edits), "{edits} edit stubs");
    }

    // The stubs persist after the turn closes: a later, small turn still
    // projects the long one under the same bound.
    rt.run_turn(
        Author::User(aigentic_core::UserId("steve".into())),
        vec![ContentBlock::Text("anything new?".into())],
        &mut |_| {},
    )
    .await
    .unwrap();
    {
        let requests = seen.lock().unwrap();
        let last = requests.last().unwrap();
        let size = estimate(last);
        assert!(size < 128_000, "the closed turn still projects at {size}");
        assert_eq!(last.last().map(|m| m.role), Some(Role::User));
    }
}
