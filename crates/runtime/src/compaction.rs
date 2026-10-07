//! The compaction seam, implemented. Runs at the top of every loop
//! iteration and on `/compact`. Everything it does is an appended
//! `compacted` event; the projection applies it.

use aigentic_core::{
    Author, CUT_STREAM, CompletionRequest, ContentBlock, Event, EventKind, Message, ProviderError,
    ProviderEvent, Role, Usage,
};
use aigentic_log::{
    CompactedPayload, CompactionStrategy, PinnedPayload, ToolResultPayload, project,
};
use futures_util::StreamExt;

use crate::decisions::CancelToken;
use crate::{Runtime, RuntimeError, Signal, build_context};

/// Fixed and versioned; the summary is only as good as this.
pub const SUMMARY_PROMPT: &str = "Summarise the conversation so far for an agent that will continue it \
without seeing the original. Keep: the user's goals and constraints, every decision and its reason, \
file paths touched and what changed in each, commands run and their outcomes, open questions and \
anything the user asked to remember. Drop: greetings, restated tool output, reasoning that led \
nowhere. Write in the past tense, as facts, under 600 words.";

/// Fixed and versioned; a link is only as good as this. The same keep/drop
/// rules as [`SUMMARY_PROMPT`], over one part of a thread that runs on as
/// a chain of links (issue #98): the turns between the previous link and
/// the keep window, which the earlier summary continues.
pub const LINK_PROMPT: &str = "Summarise this part of the conversation for an agent that will continue \
it without seeing the original, which continues the earlier summary below. Keep: the user's goals and \
constraints, every decision and its reason, file paths touched and what changed in each, commands run \
and their outcomes, open questions and anything the user asked to remember. Drop: greetings, restated \
tool output, reasoning that led nowhere. Write in the past tense, as facts, under 600 words, and do not \
repeat what the earlier summary already says.";

const SUMMARY_REQUEST: &str = "Write the summary now.";

/// How many `summary_max_output_tokens` a link range must be worth before
/// a link is considered at all (issue #98, design 2): a link pays a
/// utility call and a cache break and writes up to one cap itself, so a
/// range under about three caps is not worth summarising. The second,
/// inner threshold is the saving itself (`link_saving`).
const LINK_MIN_CAPS: u64 = 3;

/// The fill of the model's real window at which the emergency net fires
/// (issue #98, design 5): the near-limit trigger stops aiming at
/// `window_line()` and becomes the last resort for a single turn with
/// nothing closed to batch.
const EMERGENCY_FRACTION: f64 = 0.9;

impl Runtime {
    /// The emergency net (issue #98, design 5): run the compaction rules
    /// exactly as before, but only when the context nears the model's
    /// **real** window, not the working-set target. On a 1M window it
    /// never fires; on a small model it saves the turn. The normal path
    /// summarises older turns at the batch instead, and `/compact` still
    /// runs the rules on demand.
    pub async fn emergency_compact(
        &mut self,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<bool, RuntimeError> {
        let context = build_context(&self.prefix(), self.log.events())?;
        if self.fill(&context) < self.emergency_line() {
            return Ok(false);
        }
        let did = self.run_rules(false, observe).await?;
        Ok(!did.is_empty())
    }

    /// The emergency net's line: [`EMERGENCY_FRACTION`] of what the model
    /// really holds, which is at or above `window_line()` on every
    /// profile.
    pub fn emergency_line(&self) -> u64 {
        (self.provider.capabilities().max_context_tokens as f64 * EMERGENCY_FRACTION).round() as u64
    }

    /// Manual compaction (`/compact`): run the rules regardless of pressure.
    pub async fn compact_now(
        &mut self,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<Vec<CompactionStrategy>, RuntimeError> {
        self.run_rules(true, observe).await
    }

    /// Append a pinned fact. Pins live in the stable prefix.
    pub fn pin(
        &mut self,
        author: Author,
        text: String,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<Event, RuntimeError> {
        let payload = serde_json::to_value(PinnedPayload { text }).expect("serialisable");
        self.measured = None;
        self.append(EventKind::Pinned, author, payload, None, observe)
    }

    /// Tokens at which compaction triggers: the fraction of the window,
    /// capped by the profile's working-set target (issues #30, #76). On a
    /// 1M window the fraction alone would sit at 734k and never run; the
    /// target is what we are willing to pay for per call. `trigger_fraction`
    /// is the cap for small windows: the target is
    /// `min(fraction × window, working_set_tokens)`, and the target is the
    /// line the closed-turn batch aims at too.
    pub fn window_line(&self) -> u64 {
        let line = (self.provider.capabilities().max_context_tokens as f64
            * f64::from(self.compaction.trigger_fraction))
        .round() as u64;
        match self.compaction.working_set_tokens {
            0 => line,
            target => line.min(target),
        }
    }

    /// Window fill: the last call's reported prompt size — already a real
    /// count — plus what was appended since, estimated and calibrated
    /// into reported tokens (issue #52); the estimate alone, calibrated
    /// the same way, when nothing was reported or the prefix changed (a
    /// compaction or a pin resets the measure).
    pub fn fill(&self, context: &[Message]) -> u64 {
        match self.measured {
            Some((prompt, at_len)) if at_len <= context.len() => {
                prompt
                    + crate::evict::calibrated_delta(
                        self.provider.count_tokens(&context[at_len..]),
                        self.ratio,
                    )
            }
            _ => {
                let estimate = self.provider.count_tokens(context);
                crate::evict::calibrated(estimate, self.overhead, self.ratio)
            }
        }
    }

    /// Rule 1 then rule 2, each only if applicable. `force` skips the
    /// pressure check on rule 2 but never summarises the keep window.
    async fn run_rules(
        &mut self,
        force: bool,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<Vec<CompactionStrategy>, RuntimeError> {
        let mut done = Vec::new();
        let Some((from, to)) = self.compactable_range() else {
            return Ok(done);
        };

        if let Some(strategy) = self.truncate(from, to, observe)? {
            done.push(strategy);
            if !force {
                // Re-check after the truncate append, as the rules always
                // did: the cache makes the fresh read free.
                let context = build_context(&self.prefix(), self.log.events())?;
                if self.fill(&context) < self.window_line() {
                    return Ok(done);
                }
            }
        }

        if let Some(strategy) = self.summarise(from, to, observe).await? {
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
    fn compactable_range(&self) -> Option<(u64, u64)> {
        let events = self.log.events();
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

    /// The next link's range (issue #98, design 2), pure over the log:
    /// from just after everything compacted so far through the
    /// `turn_ended` that leaves `keep_turns` closed turns verbatim, or
    /// `None` when that range is empty or is not yet worth a call.
    ///
    /// `events` and `ratio` are passed rather than read off `self` for
    /// the same reason `decide_batch` takes them: a replay asks the
    /// question against the log as it stood then, with the ratio the
    /// batch's own event recorded.
    pub(crate) fn link_range(&self, events: &[Event], ratio: f64) -> Option<(u64, u64)> {
        let compacted_through = project(events).ok()?.compacted_through;
        let from = match compacted_through {
            Some(through) => through + 1,
            None => events.first()?.seq,
        };
        let ends: Vec<u64> = events
            .iter()
            .filter(|e| e.kind == EventKind::TurnEnded)
            .map(|e| e.seq)
            .collect();
        let idx = ends.len().checked_sub(self.compaction.keep_turns + 1)?;
        let to = ends[idx];
        if to < from {
            return None;
        }
        // The originals, as design 2 reads them: a link pays a call and a
        // cache break and writes up to one cap itself, so a range under
        // about three caps is not worth it.
        let tokens = self
            .slice_tokens(&self.link_inputs(events, from, to, None), ratio)
            .ok()?;
        (tokens >= LINK_MIN_CAPS * self.compaction.summary_max_output_tokens).then_some((from, to))
    }

    /// What a link over `from..=to` would free from the batch's stubbed
    /// projection (issue #98, design 4): the range's tokens there, less
    /// the cap the link itself writes, floored at zero. The batch fires on
    /// this plus what stubbing frees, so a thread that grows by text alone
    /// still batches once everything is stubbed. Public so a test can ask
    /// the question the decision asked, with the ratio it asked under.
    pub fn link_saving(
        &self,
        events: &[Event],
        from: u64,
        to: u64,
        through_seq: u64,
        ratio: f64,
    ) -> Result<u64, RuntimeError> {
        let tokens = self.slice_tokens(
            &self.link_inputs(events, from, to, Some(through_seq)),
            ratio,
        )?;
        Ok(tokens.saturating_sub(self.compaction.summary_max_output_tokens))
    }

    /// The link range's own events, as a link reads them (issue #98,
    /// design 2): only the events in `from..=to`, never the log's own
    /// `results_stubbed`, `context_evicted` or `compacted`, so a link
    /// reads its own range and not the stubbed projection or seq 0.
    /// `stubbed_at` adds the batch's synthetic `results_stubbed` instead
    /// of the synthetic truncation, which is how design 4 prices what the
    /// link would free once the batch has run.
    pub(crate) fn link_inputs(
        &self,
        events: &[Event],
        from: u64,
        to: u64,
        stubbed_at: Option<u64>,
    ) -> Vec<Event> {
        let mut slice: Vec<Event> = events
            .iter()
            .filter(|e| (from..=to).contains(&e.seq))
            // The originals, never the log's own bookkeeping (design 3):
            // a batch's `results_stubbed`, an eviction, and a `compacted`
            // inside the range — the latter so a nest that happened at the
            // range's start does not stand in for the range's own turns,
            // which the earlier-summary message carries instead.
            .filter(|e| {
                !matches!(
                    e.kind,
                    EventKind::ResultsStubbed | EventKind::ContextEvicted | EventKind::Compacted
                )
            })
            .cloned()
            .collect();
        let Some(last) = events.last().cloned() else {
            return slice;
        };
        slice.push(match stubbed_at {
            Some(through_seq) => crate::evict::synthetic_batch(&last, through_seq),
            None => synthetic_truncate(&last, from, to, self.compaction.max_result_bytes),
        });
        slice
    }

    /// The text of the newest `Summary` whose `to` is before `from` — the
    /// earlier summary a link continues, link or nest (issue #98, design
    /// 3). `None` only when `from` is the log's first event.
    pub(crate) fn previous_summary(&self, events: &[Event], from: u64) -> Option<String> {
        events
            .iter()
            .filter(|e| e.kind == EventKind::Compacted)
            .filter_map(|e| serde_json::from_value::<CompactedPayload>(e.payload.clone()).ok())
            .filter_map(|p| match p.strategy {
                CompactionStrategy::Summary { text, .. } => Some((p.to_seq, text)),
                CompactionStrategy::TruncateResults { .. } => None,
            })
            .rfind(|(to, _)| *to < from)
            .map(|(_, text)| text)
    }

    /// A slice of the log priced as `context_tokens` prices one: the
    /// estimator, calibrated into the target's units (issue #52).
    fn slice_tokens(&self, slice: &[Event], ratio: f64) -> Result<u64, RuntimeError> {
        let context = build_context(&self.prefix(), slice)?;
        let estimate = self.provider.count_tokens(&context);
        Ok(crate::evict::calibrated(estimate, self.overhead, ratio))
    }

    /// The chain's next link (issue #98, design 3): summarise the turns
    /// between the previous link and the keep window on the utility
    /// model, in one call, at the batch.
    ///
    /// Best effort: on a timeout, a provider error, a cut stream, empty
    /// text or cancellation, nothing is appended, a note says the summary
    /// was skipped, and the turn carries on — the next batch, which needs
    /// another closed turn, retries over a then-longer range. Only the
    /// log append itself can fail the turn.
    ///
    /// The call's time counts inside the turn's wall budget, which the
    /// loop checks at its next iteration (`turn.rs`), so a link can delay
    /// a `wall_time` end by up to the limit.
    pub(crate) async fn summarise_link(
        &mut self,
        from: u64,
        to: u64,
        cancel: &CancelToken,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<(), RuntimeError> {
        // Which provider runs the link (issue #98, design 3), by memory
        // extraction's rule: the utility profile's when one is
        // configured, else the thread's own.
        let model = self
            .utility_label
            .clone()
            .unwrap_or_else(|| self.model_label.clone());
        let events = self.log.events().to_vec();
        let mut messages = match project(&self.link_inputs(&events, from, to, None)) {
            Ok(p) => p.body,
            Err(e) => {
                observe(Signal::Note(format!(
                    "summary skipped: the range could not be projected ({e}); the next batch retries"
                )));
                return Ok(());
            }
        };
        let earlier_summary = self.previous_summary(&events, from);
        if messages.is_empty() {
            observe(Signal::Note(
                "summary skipped: the range holds nothing to summarise; the next batch retries"
                    .into(),
            ));
            return Ok(());
        }
        // Provider blobs are bound to the prefix that produced them and
        // mean nothing to a summary: replaying them under the link prompt
        // is rejected by Anthropic.
        for m in &mut messages {
            m.blocks
                .retain(|b| !matches!(b, ContentBlock::ProviderBlob(_)));
        }
        messages.retain(|m| !m.blocks.is_empty());
        messages.insert(0, system(LINK_PROMPT));
        if let Some(earlier) = earlier_summary {
            messages.insert(1, system(&format!("Earlier summary: {earlier}")));
        }
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

        observe(Signal::Note(format!(
            "summarising turns {from}–{to} on {model}"
        )));
        let limit = self.summary_limit;
        let (mut text, mut usage) = (String::new(), Usage::default());
        let mut skipped: Option<String> = None;
        {
            let deadline = tokio::time::sleep(limit);
            tokio::pin!(deadline);
            let mut stream = self.utility().complete(&request);
            loop {
                tokio::select! {
                    () = &mut deadline => {
                        skipped = Some(format!("timed out after {}s", limit.as_secs()));
                        break;
                    }
                    _ = cancel.cancelled() => {
                        skipped = Some("the turn was cancelled".into());
                        break;
                    }
                    event = stream.next() => match event {
                        None => break,
                        Some(ProviderEvent::TextDelta(t)) => text.push_str(&t),
                        Some(ProviderEvent::Usage(u)) => usage = u,
                        Some(ProviderEvent::Error(e)) => {
                            skipped = Some(format!("provider error: {e}"));
                            break;
                        }
                        // A link cut off mid-way says less than the
                        // originals it would replace (issue #96).
                        Some(ProviderEvent::Done { finish_reason }) if finish_reason == CUT_STREAM => {
                            skipped = Some("the stream was cut".into());
                            break;
                        }
                        Some(_) => {}
                    },
                }
            }
        }
        if let Some(reason) = skipped {
            observe(Signal::Note(format!(
                "summary skipped: {reason}; the next batch retries"
            )));
            return Ok(());
        }
        if text.trim().is_empty() {
            observe(Signal::Note(
                "summary skipped: the model wrote nothing; the next batch retries".into(),
            ));
            return Ok(());
        }
        // What that provider's table says the call cost (issue #98,
        // design 6), memory extraction's rule: the utility profile's
        // prices when a utility is configured, the thread's when the
        // thread's own provider ran it.
        let prices = if self.utility.is_some() {
            self.utility_prices
        } else {
            self.prices
        };
        let mut stamped = aigentic_log::Usage::reported(usage);
        stamped.cost_usd = prices.map(|p| p.cost_usd(&stamped));
        let strategy = CompactionStrategy::Summary {
            text: text.trim().to_owned(),
            model,
            usage: Box::new(stamped),
        };
        self.append_compaction(from, to, strategy, true, Author::System, observe)?;
        Ok(())
    }

    /// Rule 1: one truncation event over the range if any result in it is
    /// still longer than `max_result_bytes` after existing truncations.
    fn truncate(
        &mut self,
        from: u64,
        to: u64,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<Option<CompactionStrategy>, RuntimeError> {
        let max = self.compaction.max_result_bytes;
        // Everything the decision needs, computed under one short borrow
        // that ends before the append below.
        let oversize = {
            let events = self.log.events();
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
            events.iter().any(|e| {
                e.kind == EventKind::ToolResult
                    && (from..=to).contains(&e.seq)
                    && serde_json::from_value::<ToolResultPayload>(e.payload.clone())
                        .is_ok_and(|p| p.result.content.len() > max)
                    && !already
                        .iter()
                        .any(|(f, t, m)| *f <= e.seq && e.seq <= *t && *m <= max)
            })
        };
        if !oversize {
            return Ok(None);
        }
        let strategy = CompactionStrategy::TruncateResults { max_bytes: max };
        self.append_compaction(
            from,
            to,
            strategy.clone(),
            false,
            Author::Agent(self.agent.clone()),
            observe,
        )?;
        Ok(Some(strategy))
    }

    /// Rule 2: summarise the range with the same provider and a fixed prompt.
    async fn summarise(
        &mut self,
        from: u64,
        to: u64,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<Option<CompactionStrategy>, RuntimeError> {
        // The range as the model would see it, with earlier compactions
        // applied: owned, so the borrow of the log ends before the call.
        let in_range: Vec<Event> = self
            .log
            .events()
            .iter()
            .filter(|e| e.seq <= to || e.kind == EventKind::Compacted)
            .cloned()
            .collect();
        let mut messages = project(&in_range)?.body;
        if messages.is_empty() {
            return Ok(None);
        }
        // Provider blobs (thinking blocks) are bound to the prefix that
        // produced them and mean nothing to a summary: replaying them under
        // the summary prompt is rejected by Anthropic.
        for m in &mut messages {
            m.blocks
                .retain(|b| !matches!(b, ContentBlock::ProviderBlob(_)));
        }
        messages.retain(|m| !m.blocks.is_empty());
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
                // A summary cut off mid-way says less than the originals
                // and would be appended in their place (issue #96), so it
                // is a provider failure like any other.
                ProviderEvent::Done { finish_reason } if finish_reason == CUT_STREAM => {
                    return Err(RuntimeError::Provider(ProviderError::Cut));
                }
                _ => {}
            }
        }
        drop(stream);
        if text.trim().is_empty() {
            return Ok(None);
        }
        // What the provider's table says the call cost (issue #98): the
        // record already carries the model and the numbers, and the price
        // belongs beside them.
        let prices = self.prices;
        let mut usage = aigentic_log::Usage::reported(usage);
        usage.cost_usd = prices.map(|p| p.cost_usd(&usage));
        let strategy = CompactionStrategy::Summary {
            text: text.trim().to_owned(),
            model: self.model_label.clone(),
            usage: Box::new(usage),
        };
        self.append_compaction(
            from,
            to,
            strategy.clone(),
            false,
            Author::Agent(self.agent.clone()),
            observe,
        )?;
        Ok(Some(strategy))
    }

    fn append_compaction(
        &mut self,
        from: u64,
        to: u64,
        strategy: CompactionStrategy,
        continuous: bool,
        author: Author,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<(), RuntimeError> {
        let payload = serde_json::to_value(CompactedPayload {
            from_seq: from,
            to_seq: to,
            strategy,
            continuous,
        })
        .expect("serialisable");
        self.measured = None;
        self.append(EventKind::Compacted, author, payload, None, observe)?;
        Ok(())
    }
}

/// A `truncate_results` over `from..=to`, made to look like it follows
/// `after`: the link's probe, never appended to a log (issue #98, design
/// 2). A link reads its own range's originals the way rule 1 would leave
/// them, so a long result arrives cut to its head and tail.
fn synthetic_truncate(after: &Event, from: u64, to: u64, max_bytes: usize) -> Event {
    Event {
        id: ulid::Ulid::generate(),
        thread_id: after.thread_id,
        seq: after.seq + 1,
        kind: EventKind::Compacted,
        author: Author::System,
        payload: serde_json::to_value(CompactedPayload {
            from_seq: from,
            to_seq: to,
            strategy: CompactionStrategy::TruncateResults { max_bytes },
            continuous: false,
        })
        .expect("serialisable"),
        parent_event: None,
        created_at: after.created_at,
    }
}

fn system(text: &str) -> Message {
    Message {
        role: Role::System,
        author: Author::System,
        blocks: vec![ContentBlock::Text(text.to_owned())],
    }
}
