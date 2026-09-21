use std::fmt;

use aigentic_runtime::aigentic_core::{Event, EventKind};
use aigentic_runtime::aigentic_log::AssistantMessagePayload;

/// Token totals for a thread, with the estimated share kept apart.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Cost {
    pub input: u64,
    pub output: u64,
    pub estimated_input: u64,
    pub estimated_output: u64,
    pub calls: u32,
    pub estimated_calls: u32,
}

/// Sum usage over every `assistant_message` in the log.
pub fn cost_of(events: &[Event]) -> Cost {
    let mut cost = Cost::default();
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
        write!(
            f,
            "total      in {:>9}  out {:>9}",
            self.input + self.estimated_input,
            self.output + self.estimated_output
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
        ];
        let cost = cost_of(&events);
        assert_eq!(
            cost,
            Cost {
                input: 300,
                output: 30,
                estimated_input: 7,
                estimated_output: 7,
                calls: 2,
                estimated_calls: 1,
            }
        );
        let text = cost.to_string();
        assert!(text.contains("reported   in       300"), "{text}");
        assert!(text.contains("estimated  in         7"), "{text}");
        assert!(text.contains("total      in       307"), "{text}");
    }
}
