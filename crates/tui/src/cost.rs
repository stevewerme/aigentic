use std::fmt;

use aigentic_runtime::aigentic_core::{Event, EventKind};
use aigentic_runtime::aigentic_log::{
    AssistantMessagePayload, CompactedPayload, CompactionStrategy, MemoryExtractedPayload,
};

/// Token totals for a thread, with the estimated share kept apart.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Cost {
    pub input: u64,
    pub output: u64,
    pub estimated_input: u64,
    pub estimated_output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
    pub calls: u32,
    pub estimated_calls: u32,
    pub truncations: u32,
    pub summaries: u32,
    pub summary_input: u64,
    pub summary_output: u64,
    pub extractions: u32,
    pub extraction_lines: u32,
    pub extraction_input: u64,
    pub extraction_output: u64,
}

/// Sum usage over every `assistant_message` in the log.
pub fn cost_of(events: &[Event]) -> Cost {
    let mut cost = Cost::default();
    for event in events.iter().filter(|e| e.kind == EventKind::Compacted) {
        let Ok(p) = serde_json::from_value::<CompactedPayload>(event.payload.clone()) else {
            continue;
        };
        match p.strategy {
            CompactionStrategy::TruncateResults { .. } => cost.truncations += 1,
            CompactionStrategy::Summary { usage, .. } => {
                cost.summaries += 1;
                cost.summary_input +=
                    usage.input_tokens + usage.cache_read_tokens + usage.cache_write_tokens;
                cost.summary_output += usage.output_tokens;
            }
        }
    }
    for event in events
        .iter()
        .filter(|e| e.kind == EventKind::MemoryExtracted)
    {
        let Ok(p) = serde_json::from_value::<MemoryExtractedPayload>(event.payload.clone()) else {
            continue;
        };
        cost.extractions += 1;
        cost.extraction_lines += p.written.len() as u32;
        cost.extraction_input +=
            p.usage.input_tokens + p.usage.cache_read_tokens + p.usage.cache_write_tokens;
        cost.extraction_output += p.usage.output_tokens;
    }
    for event in events
        .iter()
        .filter(|e| e.kind == EventKind::AssistantMessage)
    {
        let Ok(payload) = serde_json::from_value::<AssistantMessagePayload>(event.payload.clone())
        else {
            continue;
        };
        let Some(u) = payload.usage else { continue };
        if u.estimated {
            cost.estimated_input += u.input_tokens;
            cost.estimated_output += u.output_tokens;
            cost.estimated_calls += 1;
        } else {
            cost.input += u.input_tokens;
            cost.output += u.output_tokens;
            cost.cache_read += u.cache_read_tokens;
            cost.cache_write += u.cache_write_tokens;
            cost.reasoning += u.reasoning_tokens.unwrap_or(0);
            cost.calls += 1;
        }
    }
    cost
}

impl fmt::Display for Cost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "reported   in {:>9}  out {:>9}  ({} calls)",
            self.input, self.output, self.calls
        )?;
        writeln!(
            f,
            "estimated  in {:>9}  out {:>9}  ({} calls without provider usage)",
            self.estimated_input, self.estimated_output, self.estimated_calls
        )?;
        writeln!(
            f,
            "cache      read {:>7}  write {:>7}  (zero on backends without caching)",
            self.cache_read, self.cache_write
        )?;
        if self.reasoning > 0 {
            writeln!(
                f,
                "reasoning  {:>12}  (share of reported output tokens)",
                self.reasoning
            )?;
        }
        if self.truncations + self.summaries > 0 {
            writeln!(
                f,
                "compactions {} ({} truncate, {} summary)   summary tokens in {} out {}",
                self.truncations + self.summaries,
                self.truncations,
                self.summaries,
                self.summary_input,
                self.summary_output
            )?;
        }
        if self.extractions > 0 {
            writeln!(
                f,
                "memory     {} extractions, {} lines   tokens in {} out {}",
                self.extractions,
                self.extraction_lines,
                self.extraction_input,
                self.extraction_output
            )?;
        }
        write!(
            f,
            "total      in {:>9}  out {:>9}",
            self.input
                + self.cache_read
                + self.cache_write
                + self.estimated_input
                + self.summary_input
                + self.extraction_input,
            self.output + self.estimated_output + self.summary_output + self.extraction_output
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aigentic_runtime::aigentic_core::Author;
    use aigentic_runtime::aigentic_log::Usage;
    use serde_json::json;

    fn event(kind: EventKind, payload: serde_json::Value) -> Event {
        Event {
            id: ulid::Ulid::generate(),
            thread_id: ulid::Ulid::generate(),
            seq: 0,
            kind,
            author: Author::System,
            payload,
            parent_event: None,
            created_at: time::OffsetDateTime::now_utc(),
        }
    }

    fn assistant(input: u64, output: u64, estimated: bool) -> Event {
        let payload = AssistantMessagePayload {
            blocks: vec![],
            usage: Some(Usage {
                input_tokens: input,
                output_tokens: output,
                cache_read_tokens: if estimated { 0 } else { 50 },
                cache_write_tokens: if estimated { 0 } else { 5 },
                reasoning_tokens: if estimated { None } else { Some(3) },
                estimated,
            }),
        };
        event(
            EventKind::AssistantMessage,
            serde_json::to_value(payload).unwrap(),
        )
    }

    #[test]
    fn sums_reported_and_estimated_separately() {
        let events = vec![
            event(EventKind::UserMessage, json!({"blocks": []})),
            assistant(100, 10, false),
            assistant(7, 7, true),
            assistant(200, 20, false),
            event(EventKind::TurnEnded, json!({"reason": "done"})),
            event(
                EventKind::Compacted,
                json!({"from_seq": 0, "to_seq": 4, "strategy": {"kind": "truncate_results", "max_bytes": 100}}),
            ),
            event(
                EventKind::Compacted,
                json!({"from_seq": 0, "to_seq": 4, "strategy": {"kind": "summary", "text": "s", "model": "m",
                       "usage": {"input_tokens": 1000, "output_tokens": 50, "cache_read_tokens": 0, "cache_write_tokens": 0, "reasoning_tokens": null, "estimated": false}}}),
            ),
            event(
                EventKind::MemoryExtracted,
                json!({"through_seq": 5, "written": [{"file": "decisions.md", "text": "x", "stated_by": {"kind": "system"}, "at_seq": 0}, {"file": "facts.md", "text": "y", "stated_by": {"kind": "system"}, "at_seq": 0}],
                       "model": "m", "usage": {"input_tokens": 300, "output_tokens": 20}}),
            ),
        ];
        let cost = cost_of(&events);
        assert_eq!(
            cost,
            Cost {
                input: 300,
                output: 30,
                estimated_input: 7,
                estimated_output: 7,
                cache_read: 100,
                cache_write: 10,
                reasoning: 6,
                calls: 2,
                estimated_calls: 1,
                truncations: 1,
                summaries: 1,
                summary_input: 1000,
                summary_output: 50,
                extractions: 1,
                extraction_lines: 2,
                extraction_input: 300,
                extraction_output: 20,
            }
        );
        assert!(
            cost.to_string()
                .contains("memory     1 extractions, 2 lines   tokens in 300 out 20"),
            "{cost}"
        );
        let text_all = cost.to_string();
        assert!(
            text_all
                .contains("compactions 2 (1 truncate, 1 summary)   summary tokens in 1000 out 50"),
            "{text_all}"
        );
        let text = cost.to_string();
        assert!(text.contains("reported   in       300"), "{text}");
        assert!(text.contains("estimated  in         7"), "{text}");
        assert!(
            text.contains("cache      read     100  write      10"),
            "{text}"
        );
        assert!(text.contains("reasoning             6"), "{text}");
        assert!(text.contains("total      in      1717"), "{text}");
    }
}
