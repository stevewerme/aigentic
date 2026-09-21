//! Tools the runtime answers itself because they need the log or the
//! human: `pin` and `ask_human` now, `load_skill` in the next step. They
//! appear in the specs like any tool, with class `safe`, so the model's
//! view is uniform; the tools crate never sees them.

use aigentic_core::{Author, EventKind, RiskClass, ToolCall, ToolResult, ToolSpec};
use aigentic_log::PinnedPayload;
use serde::Deserialize;
use serde_json::json;

use crate::{Runtime, RuntimeError, Signal};

pub const PIN: &str = "pin";
pub const ASK_HUMAN: &str = "ask_human";

#[derive(Debug, Deserialize)]
struct PinArgs {
    text: String,
}

#[derive(Debug, Deserialize)]
struct AskHumanArgs {
    question: String,
}

/// The harness tools' specs, in name order.
pub fn harness_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: ASK_HUMAN.into(),
            description: "Ask the human a question and wait for their typed answer. Use it when you cannot proceed without a decision only they can make.".into(),
            schema: json!({
                "type": "object",
                "properties": {"question": {"type": "string", "description": "The question, with the options if there are any."}},
                "required": ["question"]
            }),
        },
        ToolSpec {
            name: PIN.into(),
            description: "Pin a short fact to the stable prefix so it survives compaction: a decision, a constraint, a path that matters.".into(),
            schema: json!({
                "type": "object",
                "properties": {"text": {"type": "string", "description": "One line."}},
                "required": ["text"]
            }),
        },
    ]
}

/// Whether the runtime answers this tool itself.
pub fn is_harness_tool(name: &str) -> bool {
    matches!(name, PIN | ASK_HUMAN)
}

/// Every harness tool is `safe`.
pub const HARNESS_CLASS: RiskClass = RiskClass::Safe;

impl Runtime {
    /// Answer a harness tool. Called only after `policy_check` said run.
    pub(crate) fn run_harness_tool(
        &mut self,
        call: &ToolCall,
        observe: &mut dyn FnMut(Signal<'_>),
    ) -> Result<ToolResult, RuntimeError> {
        let id = call.id.clone();
        let ok = |content: String| ToolResult {
            id: id.clone(),
            content,
            is_error: false,
        };
        let err = |content: String| ToolResult {
            id: id.clone(),
            content,
            is_error: true,
        };
        Ok(match call.name.as_str() {
            PIN => match serde_json::from_value::<PinArgs>(call.args.clone()) {
                Ok(args) => {
                    let payload = serde_json::to_value(PinnedPayload { text: args.text })
                        .expect("serialisable");
                    self.measured = None;
                    self.append(
                        EventKind::Pinned,
                        Author::Agent(self.agent.clone()),
                        payload,
                        None,
                        observe,
                    )?;
                    ok("pinned".into())
                }
                Err(e) => err(format!("invalid arguments: {e}")),
            },
            ASK_HUMAN => match serde_json::from_value::<AskHumanArgs>(call.args.clone()) {
                Ok(args) => match self.approver.ask_human(&args.question) {
                    Some(answer) => ok(answer),
                    None => err("no human available; treat this as a no".into()),
                },
                Err(e) => err(format!("invalid arguments: {e}")),
            },
            other => err(format!("unknown harness tool: {other}")),
        })
    }
}
