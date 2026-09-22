//! Tools the runtime answers itself because they need the log or the
//! human: `pin`, `ask_human`, `load_skill` and `update_tasks`. They appear in the specs
//! like any tool, with class `safe`, so the model's view is uniform; the
//! tools crate never sees them.

use aigentic_core::{Author, EventKind, RiskClass, ToolCall, ToolResult, ToolSpec};
use aigentic_log::{Invoker, PinnedPayload, SkillLoadedPayload};
use aigentic_skills::Invocation;
use serde::Deserialize;
use serde_json::json;

use crate::{Runtime, RuntimeError, Signal};

pub const PIN: &str = "pin";
pub const ASK_HUMAN: &str = "ask_human";
pub const LOAD_SKILL: &str = "load_skill";
/// The model's checklist (phase 6 step 8c). The call is the record: the
/// whole list is in its arguments, so a client draws it from the call and
/// a replay of the log shows every version.
pub const UPDATE_TASKS: &str = "update_tasks";

/// The harness's standing instructions, one system block after the
/// person's global ones. A tool description alone did not make GLM 5.3
/// keep the checklist (zero calls on an explicit four-step task); this
/// line did (four and five calls in two runs).
pub const HARNESS_INSTRUCTIONS: &str = "For any request of three or more steps, call update_tasks before anything else with every step, then again as each step starts and finishes. Work one step at a time.";

/// One checklist item as the model sends it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
pub struct Task {
    pub text: String,
    #[serde(default)]
    pub state: TaskState,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    #[default]
    Pending,
    Active,
    Done,
}

#[derive(Debug, Deserialize)]
pub struct UpdateTasksArgs {
    pub tasks: Vec<Task>,
}

#[derive(Debug, Deserialize)]
struct PinArgs {
    text: String,
}

#[derive(Debug, Deserialize)]
struct AskHumanArgs {
    question: String,
}

#[derive(Debug, Deserialize)]
struct LoadSkillArgs {
    name: String,
}

/// The harness tools' specs, in name order. `load_skill` is offered only
/// when a model-invoked skill is enabled.
pub fn harness_specs(offer_load_skill: bool) -> Vec<ToolSpec> {
    let mut specs = vec![
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
            name: UPDATE_TASKS.into(),
            description: "Keep a short checklist of the work and show it to the human. For any task of three steps or more, call it first with every step, then again whenever a step starts or finishes, sending the whole list each time. Keep exactly one step active while you work, mark a step done only when it is finished and checked, and do not end your turn while a step is pending unless you say why.".into(),
            schema: json!({
                "type": "object",
                "properties": {"tasks": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "text": {"type": "string", "description": "The step, a few words."},
                            "state": {"type": "string", "enum": ["pending", "active", "done"]}
                        },
                        "required": ["text", "state"]
                    }
                }},
                "required": ["tasks"]
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
    ];
    if offer_load_skill {
        specs.push(ToolSpec {
            name: LOAD_SKILL.into(),
            description: "Load one of the skills listed in the system prompt by name. Its instructions enter the conversation; follow them for the task at hand.".into(),
            schema: json!({
                "type": "object",
                "properties": {"name": {"type": "string", "description": "The skill's name, exactly as listed."}},
                "required": ["name"]
            }),
        });
    }
    specs.sort_by(|a, b| a.name.cmp(&b.name));
    specs
}

/// The harness tools' names, for a skill's `requires` check: they are
/// always available, whatever the registry holds.
pub fn harness_names() -> Vec<String> {
    vec![
        ASK_HUMAN.into(),
        LOAD_SKILL.into(),
        PIN.into(),
        UPDATE_TASKS.into(),
    ]
}

/// Whether the runtime answers this tool itself.
pub fn is_harness_tool(name: &str) -> bool {
    matches!(name, PIN | ASK_HUMAN | LOAD_SKILL | UPDATE_TASKS)
}

/// Every harness tool is `safe`.
pub const HARNESS_CLASS: RiskClass = RiskClass::Safe;

impl Runtime {
    /// Answer a harness tool. Called only after `policy_check` said run.
    /// The author is who the result's event belongs to: the human who
    /// answered an `ask_human`, else the system, as for every other
    /// tool result.
    pub(crate) async fn run_harness_tool(
        &mut self,
        call: &ToolCall,
        cancel: &crate::CancelToken,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<(ToolResult, Author), RuntimeError> {
        let id = call.id.clone();
        let mut by = Author::System;
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
        let result = match call.name.as_str() {
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
                Ok(args) => match self.decisions.clone() {
                    Some(decisions) => {
                        let pending = crate::Pending::Human {
                            call_id: call.id.clone(),
                            question: args.question.clone(),
                        };
                        let rx = decisions.register(pending.clone());
                        observe(Signal::Waiting(&pending));
                        tokio::select! {
                            biased;
                            by = cancel.cancelled() => {
                                decisions.withdraw(&call.id);
                                err(format!(
                                    "the turn was interrupted by {} before an answer",
                                    crate::seams::author_name(&by)
                                ))
                            }
                            decided = rx => match decided {
                                Ok(crate::Answered::Human { text, by: who }) => {
                                    by = who;
                                    ok(text)
                                }
                                _ => err("no human available; treat this as a no".into()),
                            },
                        }
                    }
                    None => match self.approver.ask_human(&args.question) {
                        Some(answer) => {
                            by = self.approver.author();
                            ok(answer)
                        }
                        None => err("no human available; treat this as a no".into()),
                    },
                },
                Err(e) => err(format!("invalid arguments: {e}")),
            },
            LOAD_SKILL => match serde_json::from_value::<LoadSkillArgs>(call.args.clone()) {
                Ok(args) => match self.skills.get(&args.name) {
                    None => err(format!(
                        "unknown skill `{}`; the enabled skills are listed in the system prompt",
                        args.name
                    )),
                    // Upstream's rule: a model-invoked skill never loads a
                    // user-invoked one. The user runs those as slash commands.
                    Some(m) if m.invocation == Invocation::User => err(format!(
                        "skill `{}` is user-invoked; ask the user to run /{} instead",
                        args.name, args.name
                    )),
                    Some(_) => {
                        self.append_skill_loaded(&args.name, Invoker::Model, observe)?;
                        ok(format!(
                            "loaded skill `{}`; its instructions are now in context",
                            args.name
                        ))
                    }
                },
                Err(e) => err(format!("invalid arguments: {e}")),
            },
            UPDATE_TASKS => match serde_json::from_value::<UpdateTasksArgs>(call.args.clone()) {
                Ok(args) if args.tasks.is_empty() => err("send at least one task".into()),
                Ok(args) => {
                    let done = args
                        .tasks
                        .iter()
                        .filter(|t| t.state == TaskState::Done)
                        .count();
                    let active = args
                        .tasks
                        .iter()
                        .filter(|t| t.state == TaskState::Active)
                        .count();
                    let mut text = format!("tasks: {done} of {} done", args.tasks.len());
                    if active > 1 {
                        text.push_str("; keep one step active at a time");
                    }
                    ok(text)
                }
                Err(e) => err(format!("invalid arguments: {e}")),
            },
            other => err(format!("unknown harness tool: {other}")),
        };
        Ok((result, by))
    }

    /// Append `skill_loaded` for an enabled skill. The author is the agent
    /// for the model and the invoking user for a slash command.
    pub(crate) fn append_skill_loaded(
        &mut self,
        name: &str,
        invoked_by: Invoker,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<(), RuntimeError> {
        self.append_skill_loaded_as(name, invoked_by, Author::Agent(self.agent.clone()), observe)
    }

    pub(crate) fn append_skill_loaded_as(
        &mut self,
        name: &str,
        invoked_by: Invoker,
        author: Author,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<(), RuntimeError> {
        let manifest = self
            .skills
            .get(name)
            .ok_or_else(|| RuntimeError::UnknownSkill(name.to_owned()))?;
        let entry = self
            .skills
            .entry(name)
            .ok_or_else(|| RuntimeError::UnknownSkill(name.to_owned()))?;
        let payload = serde_json::to_value(SkillLoadedPayload {
            name: name.to_owned(),
            hash: entry.sha256.clone(),
            source: entry.source_ref(),
            body: manifest.body.clone(),
            invoked_by,
        })
        .expect("serialisable");
        self.append(EventKind::SkillLoaded, author, payload, None, observe)?;
        Ok(())
    }
}
