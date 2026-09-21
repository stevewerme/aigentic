//! docs/PLAN-phase2.md done-when 2: a 200-turn thread stays under budget.

use std::pin::Pin;
use std::sync::{Arc, Mutex};

use aigentic_core::{
    AgentId, Author, Capabilities, CompletionRequest, ContentBlock, EventKind, Message, Provider,
    ProviderEvent, Role, ToolCall, Usage,
};
use aigentic_log::{CompactedPayload, CompactionStrategy, ThreadLog};
use aigentic_runtime::{CompactionSettings, Runtime, SUMMARY_PROMPT};
use futures_core::Stream;
use serde_json::json;

mod common;
use common::EchoTool;

const WINDOW: u64 = 8_000;

/// Each request's messages, plus whether it was a summarisation call.
type Requests = Arc<Mutex<Vec<(Vec<Message>, bool)>>>;

/// Replies grow the thread: ~800 bytes of text per turn, a tool call every
/// third turn, and a short summary whenever asked with the summary prompt.
/// Reports usage from the same estimate the runtime uses, like a real
/// backend would report its own count.
struct Growing {
    requests: Requests,
    turn: Mutex<u32>,
}

fn estimate(context: &[Message]) -> u64 {
    aigentic_providers::estimate::estimate_tokens(context)
}

impl Provider for Growing {
    fn complete(
        &self,
        request: &CompletionRequest<'_>,
    ) -> Pin<Box<dyn Stream<Item = ProviderEvent> + Send + '_>> {
        let is_summary = matches!(
            request.messages.first(),
            Some(Message { role: Role::System, blocks, .. }) if blocks[0] == ContentBlock::Text(SUMMARY_PROMPT.into())
        );
        self.requests
            .lock()
            .unwrap()
            .push((request.messages.to_vec(), is_summary));
        let prompt = estimate(request.messages);
        let usage = |out: u64| {
            ProviderEvent::Usage(Usage {
                input_tokens: prompt,
                output_tokens: out,
                ..Default::default()
            })
        };

        let events = if is_summary {
            vec![
                ProviderEvent::TextDelta(
                    "Earlier turns: the user counted upward and I replied at length.".into(),
                ),
                usage(20),
                ProviderEvent::Done {
                    finish_reason: "stop".into(),
                },
            ]
        } else if request.messages.last().map(|m| m.role) == Some(Role::Tool) {
            vec![
                ProviderEvent::TextDelta("Tool done. ".repeat(60)),
                usage(200),
                ProviderEvent::Done {
                    finish_reason: "stop".into(),
                },
            ]
        } else {
            let mut turn = self.turn.lock().unwrap();
            *turn += 1;
            if (*turn).is_multiple_of(3) {
                vec![
                    ProviderEvent::ToolCall(ToolCall {
                        id: format!("call_{turn}"),
                        name: "echo".into(),
                        args: json!({"msg": "x".repeat(3000)}),
                    }),
                    usage(50),
                    ProviderEvent::Done {
                        finish_reason: "tool_calls".into(),
                    },
                ]
            } else {
                vec![
                    ProviderEvent::TextDelta(format!("Reply {turn}. ").repeat(80)),
                    usage(200),
                    ProviderEvent::Done {
                        finish_reason: "stop".into(),
                    },
                ]
            }
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
            max_context_tokens: WINDOW,
        }
    }
}

#[tokio::test]
async fn two_hundred_turns_stay_under_budget() {
    let dir = tempfile::tempdir().unwrap();
    let requests: Requests = Arc::new(Mutex::new(Vec::new()));
    let provider = Growing {
        requests: requests.clone(),
        turn: Mutex::new(0),
    };
    let calls = Arc::new(Mutex::new(Vec::new()));
    let log = ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap();
    let settings = CompactionSettings {
        trigger_fraction: 0.7,
        keep_turns: 4,
        max_result_bytes: 500,
        summary_max_output_tokens: 256,
    };
    let mut rt = Runtime::new(
        Box::new(provider),
        vec![Box::new(EchoTool(calls))],
        log,
        AgentId("worker".into()),
    )
    .with_instructions(Some("Be terse.".into()))
    .with_compaction(settings)
    .with_model_label("scripted");
    let line = rt.window_line();
    assert_eq!(line, 5_600);

    for i in 0..200 {
        let outcome = rt
            .run_turn(
                Author::User(aigentic_core::UserId("steve".into())),
                vec![ContentBlock::Text(format!("turn {i}"))],
                &mut |_| {},
            )
            .await
            .unwrap();
        assert_eq!(outcome.reason, "done", "turn {i}");
    }

    let events = rt.log().read_all().unwrap();
    let compactions: Vec<CompactedPayload> = events
        .iter()
        .filter(|e| e.kind == EventKind::Compacted)
        .map(|e| serde_json::from_value(e.payload.clone()).unwrap())
        .collect();
    let summaries = compactions
        .iter()
        .filter(|c| matches!(c.strategy, CompactionStrategy::Summary { .. }))
        .count();
    let truncations = compactions.len() - summaries;
    assert!(summaries >= 1, "at least one summary; got {summaries}");
    assert!(
        truncations >= 1,
        "at least one truncation; got {truncations}"
    );

    // Every range ends on a turn boundary and none splits a call from its result.
    for c in &compactions {
        assert_eq!(
            events[c.to_seq as usize].kind,
            EventKind::TurnEnded,
            "{c:?}"
        );
        if let CompactionStrategy::Summary { model, .. } = &c.strategy {
            assert_eq!(model, "scripted");
        }
    }

    let reqs = requests.lock().unwrap();
    let mut model_calls = 0;
    for (messages, is_summary) in reqs.iter() {
        if *is_summary {
            continue;
        }
        model_calls += 1;
        let size = estimate(messages);
        assert!(
            size < line,
            "request of {size} tokens is over the {line} line"
        );
        // Tool contract: every assistant tool call has a following tool result.
        let mut open: Vec<String> = Vec::new();
        for m in messages {
            for b in &m.blocks {
                match b {
                    ContentBlock::ToolCall(c) => open.push(c.id.clone()),
                    ContentBlock::ToolResult(r) => open.retain(|id| id != &r.id),
                    _ => {}
                }
            }
        }
        assert!(open.is_empty(), "unanswered calls {open:?}");
    }
    assert!(model_calls >= 200);

    // The last four turns are verbatim in the final request.
    let (last, _) = reqs.iter().rev().find(|(_, s)| !s).unwrap();
    let text = last
        .iter()
        .flat_map(|m| m.blocks.iter())
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    for i in 196..200 {
        assert!(
            text.contains(&format!("turn {i}")),
            "turn {i} missing from the final request"
        );
    }
    assert!(
        text.contains("[Summary of events"),
        "the final request carries a summary"
    );
    assert!(
        !text.contains("turn 10\n"),
        "early turns are summarised away"
    );

    // Summaries are auditable and cost is attributed.
    let summary_usage: u64 = compactions
        .iter()
        .filter_map(|c| match &c.strategy {
            CompactionStrategy::Summary { usage, .. } => Some(usage.output_tokens),
            _ => None,
        })
        .sum();
    assert!(summary_usage > 0);
}

#[tokio::test]
async fn compact_now_reports_what_it_did_and_pin_lands_in_the_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let requests: Requests = Arc::new(Mutex::new(Vec::new()));
    let provider = Growing {
        requests: requests.clone(),
        turn: Mutex::new(0),
    };
    let log = ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap();
    let mut rt = Runtime::new(Box::new(provider), vec![], log, AgentId("worker".into()))
        .with_compaction(CompactionSettings {
            keep_turns: 1,
            ..aigentic_runtime::DEFAULT_COMPACTION
        });
    let steve = Author::User(aigentic_core::UserId("steve".into()));

    assert!(
        rt.compact_now(&mut |_| {}).await.unwrap().is_empty(),
        "nothing to compact yet"
    );
    rt.pin(steve.clone(), "Answer in Swedish.".into(), &mut |_| {})
        .unwrap();
    for i in 0..3 {
        rt.run_turn(
            steve.clone(),
            vec![ContentBlock::Text(format!("t{i}"))],
            &mut |_| {},
        )
        .await
        .unwrap();
    }
    let did = rt.compact_now(&mut |_| {}).await.unwrap();
    assert_eq!(did.len(), 1, "{did:?}");
    assert!(matches!(did[0], CompactionStrategy::Summary { .. }));

    rt.run_turn(steve, vec![ContentBlock::Text("t3".into())], &mut |_| {})
        .await
        .unwrap();
    let reqs = requests.lock().unwrap();
    let (last, _) = reqs.last().unwrap();
    assert_eq!(last[0].role, Role::System);
    assert!(
        matches!(&last[0].blocks[0], ContentBlock::Text(t) if t.contains("Pinned facts:\n- Answer in Swedish."))
    );
    assert!(
        matches!(&last[1].blocks[0], ContentBlock::Text(t) if t.starts_with("[Summary of events 0 to"))
    );
}
