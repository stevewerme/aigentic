//! The compaction seam, implemented. Runs at the top of every loop
//! iteration and on `/compact`. Everything it does is an appended
//! `compacted` event; the projection applies it.

use aigentic_core::{
    Author, CompletionRequest, ContentBlock, Event, EventKind, Message, ProviderEvent, Role, Usage,
};
use aigentic_log::{
    CompactedPayload, CompactionStrategy, PinnedPayload, ToolResultPayload, project,
};
use futures_util::StreamExt;

use crate::{Runtime, RuntimeError, Signal, build_context};

/// Fixed and versioned; the summary is only as good as this.
pub const SUMMARY_PROMPT: &str = "Summarise the conversation so far for an agent that will continue it \
without seeing the original. Keep: the user's goals and constraints, every decision and its reason, \
file paths touched and what changed in each, commands run and their outcomes, open questions and \
anything the user asked to remember. Drop: greetings, restated tool output, reasoning that led \
nowhere. Write in the past tense, as facts, under 600 words.";

const SUMMARY_REQUEST: &str = "Write the summary now.";

impl Runtime {
    /// The seam: compact if the window is under pressure. Returns whether
    /// anything was appended.
    pub async fn compact(
        &mut self,
        observe: &mut dyn FnMut(Signal<'_>),
    ) -> Result<bool, RuntimeError> {
        let events = self.log.read_all()?;
        let context = build_context(self.instructions.as_deref(), &events)?;
        if self.fill(&context) < self.window_line() {
            return Ok(false);
        }
        let did = self.run_rules(&events, false, observe).await?;
        Ok(!did.is_empty())
    }

    /// Manual compaction (`/compact`): run the rules regardless of pressure.
    pub async fn compact_now(
        &mut self,
        observe: &mut dyn FnMut(Signal<'_>),
    ) -> Result<Vec<CompactionStrategy>, RuntimeError> {
        let events = self.log.read_all()?;
        self.run_rules(&events, true, observe).await
    }

    /// Append a pinned fact. Pins live in the stable prefix.
    pub fn pin(
        &mut self,
        author: Author,
        text: String,
        observe: &mut dyn FnMut(Signal<'_>),
    ) -> Result<Event, RuntimeError> {
        let payload = serde_json::to_value(PinnedPayload { text }).expect("serialisable");
        self.measured = None;
        self.append(EventKind::Pinned, author, payload, None, observe)
    }

    /// Tokens at which compaction triggers.
    pub fn window_line(&self) -> u64 {
        let window = self.provider.capabilities().max_context_tokens as f64;
        (window * f64::from(self.compaction.trigger_fraction)).round() as u64
    }

    /// Window fill: the last call's reported prompt size plus an estimate of
    /// what was appended since; the estimate alone when nothing was reported
    /// or the prefix changed (a compaction or a pin resets the measure).
    pub fn fill(&self, context: &[Message]) -> u64 {
        match self.measured {
            Some((prompt, at_len)) if at_len <= context.len() => {
                prompt + self.provider.count_tokens(&context[at_len..])
            }
            _ => self.provider.count_tokens(context),
        }
    }

    /// Rule 1 then rule 2, each only if applicable. `force` skips the
    /// pressure check on rule 2 but never summarises the keep window.
    async fn run_rules(
        &mut self,
        events: &[Event],
        force: bool,
        observe: &mut dyn FnMut(Signal<'_>),
    ) -> Result<Vec<CompactionStrategy>, RuntimeError> {
        let mut done = Vec::new();
        let Some((from, to)) = self.compactable_range(events) else {
            return Ok(done);
        };

        if let Some(strategy) = self.truncate(events, from, to, observe)? {
            done.push(strategy);
            if !force {
                let events = self.log.read_all()?;
                let context = build_context(self.instructions.as_deref(), &events)?;
                if self.fill(&context) < self.window_line() {
                    return Ok(done);
                }
            }
        }

        if let Some(strategy) = self.summarise(events, from, to, observe).await? {
            done.push(strategy);
        }
        Ok(done)
    }

    /// From seq 0 through the `turn_ended` that leaves exactly `keep_turns`
    /// complete turns after it. Every summary starts at 0 and nests over
    /// the previous one (whose text is part of its input), so the
    /// projection always holds exactly one summary message; chaining
    /// summaries would let them accumulate. `None` when the boundary has
    /// not moved since the last summary, so the same range is never
    /// summarised twice.
    fn compactable_range(&self, events: &[Event]) -> Option<(u64, u64)> {
        let compacted_through = project(events).ok()?.compacted_through;
        let ends: Vec<u64> = events
            .iter()
            .filter(|e| e.kind == EventKind::TurnEnded)
            .map(|e| e.seq)
            .collect();
        let idx = ends.len().checked_sub(self.compaction.keep_turns + 1)?;
        let to = ends[idx];
        (compacted_through.is_none_or(|t| t < to)).then_some((0, to))
    }

    /// Rule 1: one truncation event over the range if any result in it is
    /// still longer than `max_result_bytes` after existing truncations.
    fn truncate(
        &mut self,
        events: &[Event],
        from: u64,
        to: u64,
        observe: &mut dyn FnMut(Signal<'_>),
    ) -> Result<Option<CompactionStrategy>, RuntimeError> {
        let max = self.compaction.max_result_bytes;
        let already: Vec<(u64, u64, usize)> = events
            .iter()
            .filter(|e| e.kind == EventKind::Compacted)
            .filter_map(|e| serde_json::from_value::<CompactedPayload>(e.payload.clone()).ok())
            .filter_map(|p| match p.strategy {
                CompactionStrategy::TruncateResults { max_bytes } => {
                    Some((p.from_seq, p.to_seq, max_bytes))
                }
                _ => None,
            })
            .collect();
        let oversize = events.iter().any(|e| {
            e.kind == EventKind::ToolResult
                && (from..=to).contains(&e.seq)
                && serde_json::from_value::<ToolResultPayload>(e.payload.clone())
                    .is_ok_and(|p| p.0.content.len() > max)
                && !already
                    .iter()
                    .any(|(f, t, m)| *f <= e.seq && e.seq <= *t && *m <= max)
        });
        if !oversize {
            return Ok(None);
        }
        let strategy = CompactionStrategy::TruncateResults { max_bytes: max };
        self.append_compaction(from, to, strategy.clone(), observe)?;
        Ok(Some(strategy))
    }

    /// Rule 2: summarise the range with the same provider and a fixed prompt.
    async fn summarise(
        &mut self,
        events: &[Event],
        from: u64,
        to: u64,
        observe: &mut dyn FnMut(Signal<'_>),
    ) -> Result<Option<CompactionStrategy>, RuntimeError> {
        // The range as the model would see it, with earlier compactions applied.
        let in_range: Vec<Event> = events
            .iter()
            .filter(|e| e.seq <= to || e.kind == EventKind::Compacted)
            .cloned()
            .collect();
        let mut messages = project(&in_range)?.body;
        if messages.is_empty() {
            return Ok(None);
        }
        messages.insert(0, system(SUMMARY_PROMPT));
        messages.push(Message {
            role: Role::User,
            author: Author::System,
            blocks: vec![ContentBlock::Text(SUMMARY_REQUEST.into())],
        });
        let request = CompletionRequest {
            messages: &messages,
            tools: &[],
            max_output_tokens: Some(self.compaction.summary_max_output_tokens),
        };

        let (mut text, mut usage) = (String::new(), Usage::default());
        let mut stream = self.provider.complete(&request);
        while let Some(event) = stream.next().await {
            match event {
                ProviderEvent::TextDelta(t) => text.push_str(&t),
                ProviderEvent::Usage(u) => usage = u,
                ProviderEvent::Error(e) => return Err(RuntimeError::Provider(e)),
                _ => {}
            }
        }
        drop(stream);
        if text.trim().is_empty() {
            return Ok(None);
        }
        let strategy = CompactionStrategy::Summary {
            text: text.trim().to_owned(),
            model: self.model_label.clone(),
            usage: aigentic_log::Usage::reported(usage),
        };
        self.append_compaction(from, to, strategy.clone(), observe)?;
        Ok(Some(strategy))
    }

    fn append_compaction(
        &mut self,
        from: u64,
        to: u64,
        strategy: CompactionStrategy,
        observe: &mut dyn FnMut(Signal<'_>),
    ) -> Result<(), RuntimeError> {
        let payload = serde_json::to_value(CompactedPayload {
            from_seq: from,
            to_seq: to,
            strategy,
        })
        .expect("serialisable");
        self.measured = None;
        self.append(
            EventKind::Compacted,
            Author::Agent(self.agent.clone()),
            payload,
            None,
            observe,
        )?;
        Ok(())
    }
}

fn system(text: &str) -> Message {
    Message {
        role: Role::System,
        author: Author::System,
        blocks: vec![ContentBlock::Text(text.to_owned())],
    }
}
