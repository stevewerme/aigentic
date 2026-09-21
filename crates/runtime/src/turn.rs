//! The loop. Kept small on purpose; behaviour that is not the loop lives
//! behind the seams in `seams.rs`.

use std::time::Instant;

use aigentic_core::{
    Author, CompletionRequest, ContentBlock, EventKind, ProviderEvent, ToolCall, ToolResult,
    ToolSpec,
};
use aigentic_log::{AssistantMessagePayload, ToolResultPayload, Usage, UserMessagePayload};
use futures_util::StreamExt;

use crate::seams::PolicyDecision;
use crate::support::{Spent, flush_text};
use crate::{Runtime, RuntimeError, Signal, TurnOutcome, build_context};

impl Runtime {
    /// Run one turn: append the user message, then iterate until the model
    /// returns no tool calls or the budget is hit. Every outcome, including
    /// a budget stop or a provider failure, ends with a `turn_ended` event.
    pub async fn run_turn(
        &mut self,
        author: Author,
        blocks: Vec<ContentBlock>,
        observe: &mut dyn FnMut(Signal<'_>),
    ) -> Result<TurnOutcome, RuntimeError> {
        let mut spent = Spent {
            started: Instant::now(),
            iterations: 0,
            tokens: 0,
        };
        let payload = serde_json::to_value(UserMessagePayload { blocks }).expect("serialisable");
        self.append(EventKind::UserMessage, author, payload, None, observe)?;
        let specs: Vec<ToolSpec> = self
            .tools
            .iter()
            .map(|t| ToolSpec::from(t.as_ref()))
            .collect();

        loop {
            // Seam: an interrupt from the turn queue would be handled here.
            let _ = self.turn_queue_next();
            if let Some(reason) = self.budget_reason(&spent) {
                return self.end_turn(reason, &spent, observe);
            }
            let events = self.log.read_all()?;
            let context = self.compact(build_context(self.instructions.as_deref(), &events)?);
            let request = CompletionRequest {
                messages: &context,
                tools: &specs,
                max_output_tokens: None,
            };

            let (mut blocks, mut text, mut usage, mut error) =
                (Vec::new(), String::new(), None, None);
            let mut stream = self.provider.complete(&request);
            while let Some(event) = stream.next().await {
                match event {
                    ProviderEvent::TextDelta(t) => {
                        observe(Signal::TextDelta(&t));
                        text.push_str(&t);
                    }
                    ProviderEvent::ToolCall(call) => {
                        flush_text(&mut text, &mut blocks);
                        blocks.push(ContentBlock::ToolCall(call));
                    }
                    ProviderEvent::Blob(blob) => blocks.push(ContentBlock::ProviderBlob(blob)),
                    ProviderEvent::Usage {
                        input_tokens,
                        output_tokens,
                    } => {
                        usage = Some(Usage {
                            input_tokens,
                            output_tokens,
                        });
                    }
                    ProviderEvent::Done { .. } => {}
                    ProviderEvent::Error(e) => {
                        error = Some(e);
                        break;
                    }
                }
            }
            drop(stream);
            flush_text(&mut text, &mut blocks);
            spent.iterations += 1;
            spent.tokens += usage
                .map(|u| u.input_tokens + u.output_tokens)
                .unwrap_or_else(|| self.provider.count_tokens(&context));
            if let Some(e) = error {
                self.end_turn(&format!("provider_error: {e}"), &spent, observe)?;
                return Err(RuntimeError::Provider(e));
            }

            let calls: Vec<ToolCall> = blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolCall(c) => Some(c.clone()),
                    _ => None,
                })
                .collect();
            let payload = serde_json::to_value(AssistantMessagePayload { blocks, usage })
                .expect("serialisable");
            let agent = Author::Agent(self.agent.clone());
            let assistant =
                self.append(EventKind::AssistantMessage, agent, payload, None, observe)?;
            if calls.is_empty() {
                return self.end_turn("done", &spent, observe);
            }

            for call in calls {
                observe(Signal::ToolCallStarted(&call));
                let result = match self.policy_check(&call) {
                    PolicyDecision::Allowed => self.run_tool(&call).await,
                    PolicyDecision::Denied(reason) => ToolResult {
                        id: call.id.clone(),
                        content: reason,
                        is_error: true,
                    },
                };
                let payload =
                    serde_json::to_value(ToolResultPayload(result)).expect("serialisable");
                self.append(
                    EventKind::ToolResult,
                    Author::System,
                    payload,
                    Some(assistant.id),
                    observe,
                )?;
            }
        }
    }
}
