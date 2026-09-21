//! The audit behind done-when 2: every `tool_result` carries the policy
//! record that let it run or refused it.

use aigentic_core::{Event, EventKind};
use aigentic_log::ToolResultPayload;

/// Seqs of `tool_result` events with no policy record, or whose payload
/// does not parse. Empty means the log passes.
pub fn audit_tool_results(events: &[Event]) -> Vec<u64> {
    events
        .iter()
        .filter(|e| e.kind == EventKind::ToolResult)
        .filter(|e| {
            serde_json::from_value::<ToolResultPayload>(e.payload.clone())
                .map(|p| p.policy.is_none())
                .unwrap_or(true)
        })
        .map(|e| e.seq)
        .collect()
}
