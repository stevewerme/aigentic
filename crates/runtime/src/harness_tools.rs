//! Tools the runtime answers itself because they need the log or the
//! human: `pin`, `ask_human`, `load_skill` and `update_tasks`. They appear in the specs
//! like any tool, with class `safe`, so the model's view is uniform; the
//! tools crate never sees them.

use aigentic_core::{
    Author, ContentBlock, Event, EventKind, RiskClass, ToolCall, ToolResult, ToolSpec,
};
use aigentic_log::{
    AssistantMessagePayload, CommitRef, DecisionAnswer, DecisionKind, DecisionProposedPayload,
    DecisionStage, Finding, FixSize, Handoff, Invoker, LedgerEntry, PinnedPayload, PlannedTest,
    ReleaseImpact, ReportStatus, Route, SkillLoadedPayload, StepReport, ToolResultPayload, Verdict,
};
use aigentic_skills::Invocation;
use serde::Deserialize;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

use crate::{Runtime, RuntimeError, Signal};

pub const PIN: &str = "pin";
pub const ASK_HUMAN: &str = "ask_human";
pub const LOAD_SKILL: &str = "load_skill";
/// The model's checklist (phase 6 step 8c). The call is the record: the
/// whole list is in its arguments, so a client draws it from the call and
/// a replay of the log shows every version.
pub const UPDATE_TASKS: &str = "update_tasks";
/// How many completed tool calls may pass in the open turn after the model
/// last set its checklist before the harness reminds it (issue #102). The
/// model sets the list and then rarely calls `update_tasks` again — the
/// checklist sat at `0/N` for hundreds of events — so the harness
/// reminds it: at this distance, and as the very last message of the
/// context sent, a reminder is cheap and leaves no trace in the log.
/// The counts behind the number are in #102's body; 25 is well above the
/// handful of calls a short turn takes, so a turn that is not actually
/// running long stays silent.
pub const TASK_REMINDER_CALLS: usize = 25;
/// Bringing back what the harness forgot (issue #75): one dropped result
/// by its handle, a range of the log, or a search over the thread. Always
/// offered, in every thread.
pub const RECALL: &str = "recall";
/// The step thread's report (issue #55, PLAN-layer2 §2). Offered in a
/// step thread and nowhere else; a call in an ordinary thread is
/// refused with a result, and no `StepReported` event is written.
/// The refusal of a `finish_step` call with no prose in `body`.
pub const BODY_REQUIRED: &str = "body is required: the report's prose, which the runner posts under the step's heading; put it in `body`, not inside `slots`";

pub const FINISH_STEP: &str = "finish_step";
/// The project proposal (issue #7): the model proposes moving this
/// thread to another project, the turn parks, and a person answers.
/// Offered only in an ordinary thread; a step thread never sees the
/// spec, and the switch itself is refused there.
pub const SUGGEST_PROJECT: &str = "suggest_project";
/// Opening a sibling's brief (issue #123): the one-line in the projects
/// block is always there, the body on demand. Offered only when some
/// project in reach other than this thread's own carries a brief.
pub const READ_BRIEF: &str = "read_brief";
pub const READ_BRIEF_DESCRIPTION: &str = "Read a project's brief: what it is, who it is for and where it stands, the same text this thread's own project carries inline. Call it with a project in reach, by the name the system prompt lists, before working on or messaging that project.";
/// What an unoffered `read_brief` call answers (issue #123): the tool is
/// always answered, so a caller that emits it anyway is told why there is
/// nothing to read rather than left with an unknown-tool error.
pub const NO_BRIEF_IN_REACH: &str = "no project in reach has a brief";

/// What `suggest_project`'s description says (issue #7): call it first
/// and alone, the person decides, and a no leaves the thread where it
/// is.
pub const SUGGEST_PROJECT_DESCRIPTION: &str = "Propose moving this thread to another project, when the person's message belongs there. Call it first, alone; the person answers.";

/// What `recall`'s description says (issue #75): the three forms, one per
/// call, and what comes back for old text. The stubs and the truncation
/// markers the harness writes name this tool, so the model meets it in
/// the thread before it meets the spec.
pub const RECALL_DESCRIPTION: &str = "Bring back something from earlier in this thread: a dropped result by its number, a range of the log, or a search.";

/// The harness's standing instructions, one system block after the
/// person's global ones. A tool description alone did not make GLM 5.3
/// keep the checklist (zero calls on an explicit four-step task); this
/// line did (four and five calls in two runs).
pub const HARNESS_INSTRUCTIONS: &str = "For any request of three or more steps, call update_tasks before anything else with every step, then again as each step starts and finishes. Work one step at a time. A user message that arrives mid-turn is a correction or an addition from the person: read it before your next step and adjust your plan. Ask the human only through ask_human, with every question in the call (never \"answer the questions above\") and options when the answer is a choice; the client adds an Other row, so never list one yourself. The shell already starts in the project root and keeps its working directory between calls: do not cd to a guessed path. When a message belongs to another project under \"Projects in reach\", call suggest_project before anything else, as the only call in that reply.";

/// A `suggest_project` call's arguments (issue #7). Both are required,
/// and an unknown key is `invalid arguments: …` like every other
/// harness call.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuggestProjectArgs {
    /// The project to switch to, by name.
    pub project: String,
    /// One line on why the message belongs there.
    pub reason: String,
}

/// A `read_brief` call's arguments (issue #123).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadBriefArgs {
    /// A project in reach, by the name the system prompt lists it under.
    pub project: String,
}

/// A brief as the model reads it: the read-only marker, the project and
/// the text, cut at the tools' output cap — a brief is a file read, so it
/// obeys the cap every other tool obeys.
pub fn brief_result(project: &str, text: &str) -> String {
    aigentic_tools::truncate_output(
        &format!("[read-only · {project}]\n{text}"),
        aigentic_tools::DEFAULT_OUTPUT_CAP,
    )
}

/// The proposal as a person reads it in the log (`switch to getscale/site`).
pub fn proposal_text(project: &str) -> String {
    format!("switch to {project}")
}

/// The tool result when the proposal names the project the thread is
/// already in: nothing to decide, nothing to record.
pub fn already_in_text(project: &str) -> String {
    format!("already in {project}")
}

/// The tool result when a person said yes and the switch landed.
pub fn switched_text(project: &str) -> String {
    format!("switched to {project}; continuing there")
}

/// The tool result when a person said no.
pub fn declined_text(project: &str) -> String {
    format!("declined; continuing in {project}")
}

/// The tool result when a person said no and the thread is in no project
/// at all: there is no name to stay in.
pub const DECLINED_NO_PROJECT: &str = "declined; continuing here";

/// The tool result when the person said the work belongs somewhere else.
/// The correction is the useful part, so it is the whole answer.
pub fn corrected_text(where_it_belongs: &str) -> String {
    format!("declined; the person says it belongs to: {where_it_belongs}")
}

/// The tool result when nobody answered: an interrupted turn, a bare
/// runtime with no one to ask, or a `Yes` the switch itself refused.
pub fn not_answered_text(note: &str) -> String {
    format!("not answered: {note}")
}

/// A switch that a `Yes` landed on but that could not happen: the note
/// on the `withdrawn` answer, and the tool result beside it.
pub fn switch_failed(reason: &str) -> String {
    format!("switch failed: {reason}")
}

/// The note on the `withdrawn` answer when no `Decisions` is installed:
/// a bare runtime has nobody to ask.
pub const NO_ONE_TO_ANSWER: &str = "no one to answer";

/// The note on the `withdrawn` answer when the turn was interrupted
/// while the proposal waited.
pub const TURN_INTERRUPTED: &str = "turn interrupted";

/// The note on the `withdrawn` answer when a `Yes` arrived with no
/// context to switch to: a client that answered without building one.
pub const NO_CONTEXT: &str = "no context for the switch";

/// The result every other call in a batch gets: the proposal must stand
/// alone, and nothing may run in a project the person has not answered
/// for yet. Named here so a client or a test asserts the constant, not a
/// retyped copy.
pub const NOT_RUN_SIBLING: &str =
    "not run: suggest_project must be the only call; call it again after the person answers";

/// The policy record's rule text for that refusal, as the interrupt path
/// names its own.
pub const NOT_RUN_SOLO: &str = "suggest_project must be the only call";

/// The result every call in a reply gets when the model's own output
/// limit stopped it (issue #96): the arguments arrived mid-sentence, so
/// nothing runs. The shape is the interrupt path's. Named here so a
/// client or a test asserts the constant, not a retyped copy.
pub const NOT_RUN_LENGTH: &str = "not run: the reply hit the model's output limit";

/// The policy record's rule text for that refusal, as the interrupt path
/// names its own.
pub const NOT_RUN_OVER_LIMIT: &str = "the reply hit the model's output limit";

/// A `suggest_project` call's arguments, parsed. An unknown key, or a
/// missing one, is `invalid arguments: …`.
fn suggest_project_args(args: &serde_json::Value) -> Result<SuggestProjectArgs, String> {
    serde_json::from_value::<SuggestProjectArgs>(args.clone())
        .map_err(|e| format!("invalid arguments: {e}"))
}

/// A tool result that says what happened, and succeeded.
fn said(id: &str, content: String) -> ToolResult {
    ToolResult {
        id: id.to_owned(),
        content,
        is_error: false,
    }
}

/// A tool result that reports a failure, the note as its whole text.
fn refused(id: &str, note: &str) -> ToolResult {
    ToolResult {
        id: id.to_owned(),
        content: note.to_owned(),
        is_error: true,
    }
}

/// One option an `ask_human` question offers.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
pub struct HumanOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// One question an `ask_human` call asks.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct HumanQuestion {
    pub question: String,
    /// A short name for the answer line, e.g. `colour`, so a
    /// multi-question answer reads as `colour: red`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<HumanOption>,
    /// Allow picking several options.
    #[serde(default, skip_serializing_if = "is_false")]
    pub multi: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// The questions as one plain text, for the waiting lines and for a
/// client that does not know the shape.
pub fn plain_question(questions: &[HumanQuestion]) -> String {
    questions
        .iter()
        .map(|q| q.question.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

/// An `ask_human` call's arguments: every question in the call, or the
/// old single `question`, which normalises to one question with no
/// options so old logs and old models parse the same.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskHumanArgs {
    pub questions: Vec<HumanQuestion>,
}

impl<'de> Deserialize<'de> for AskHumanArgs {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            #[serde(default)]
            question: Option<String>,
            #[serde(default)]
            questions: Option<Vec<HumanQuestion>>,
        }
        let raw = Raw::deserialize(d)?;
        let questions = match raw.questions {
            Some(qs) if !qs.is_empty() => qs,
            _ => match raw.question {
                Some(q) => vec![HumanQuestion {
                    question: q,
                    header: None,
                    options: Vec::new(),
                    multi: false,
                }],
                None => {
                    return Err(serde::de::Error::custom(
                        "send 1-4 questions in `questions`, or the old `question`",
                    ));
                }
            },
        };
        if questions.len() > 4 {
            return Err(serde::de::Error::custom(
                "at most 4 questions per call; ask the rest in the next one",
            ));
        }
        Ok(Self { questions })
    }
}

/// One checklist item as the model sends it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct UpdateTasksArgs {
    pub tasks: Vec<Task>,
}

/// The open turn's own checklist: what its latest successful
/// `update_tasks` call said, and how much work has landed since.
///
/// Pure over the log, and read only for the open turn (everything after
/// the last `turn_ended`): the latest `update_tasks` call there whose
/// result is not an error — a refused call, with an unknown key or an
/// empty list, is not a checklist. The call is the record: the whole list
/// is in its arguments, so a later successful call replaces it and no
/// event is written for a change.
///
/// `None` when the open turn has no successful `update_tasks` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checklist {
    /// The steps whose state is `Done`.
    pub done: usize,
    /// The steps in the list.
    pub total: usize,
    /// The text of the `Active` step, else of the first step that is not
    /// `Done`.
    pub active: Option<String>,
    /// The tool results that landed since the checklist's own result,
    /// other `update_tasks` results excluded, because a later successful
    /// one is the latest by then and a failed one is no progress.
    pub since: usize,
}

/// The open turn's checklist, or `None` when it set none. See
/// [`Checklist`] for what the counts mean.
pub fn open_checklist(events: &[Event]) -> Option<Checklist> {
    let start = events
        .iter()
        .rposition(|e| e.kind == EventKind::TurnEnded)
        .map_or(0, |i| i + 1);
    let turn = &events[start..];

    // The turn's results in log order: where each sits, which call it
    // answers and whether it failed.
    let results: Vec<(usize, String, bool)> = turn
        .iter()
        .enumerate()
        .filter(|(_, e)| e.kind == EventKind::ToolResult)
        .filter_map(|(i, e)| {
            let payload = serde_json::from_value::<ToolResultPayload>(e.payload.clone()).ok()?;
            Some((i, payload.result.id, payload.result.is_error))
        })
        .collect();

    // The id of every `update_tasks` call in the turn, so a result of one
    // is not counted as work the model did on the checklist's behalf.
    let mut task_calls: BTreeSet<String> = BTreeSet::new();
    // The latest successful call: when it was answered, and its own list.
    let mut latest: Option<(usize, UpdateTasksArgs)> = None;
    for e in turn
        .iter()
        .filter(|e| e.kind == EventKind::AssistantMessage)
    {
        let Ok(payload) = serde_json::from_value::<AssistantMessagePayload>(e.payload.clone())
        else {
            continue;
        };
        let calls = payload.blocks.iter().filter_map(|b| match b {
            ContentBlock::ToolCall(c) if c.name == UPDATE_TASKS => Some(c),
            _ => None,
        });
        for call in calls {
            task_calls.insert(call.id.clone());
            let Some((at, _, false)) = results.iter().find(|(_, id, _)| *id == call.id) else {
                continue;
            };
            let Ok(args) = serde_json::from_value::<UpdateTasksArgs>(call.args.clone()) else {
                continue;
            };
            latest = Some((*at, args));
        }
    }

    let (at, args) = latest?;
    let done = args
        .tasks
        .iter()
        .filter(|t| t.state == TaskState::Done)
        .count();
    let active = args
        .tasks
        .iter()
        .find(|t| t.state == TaskState::Active)
        .or_else(|| args.tasks.iter().find(|t| t.state != TaskState::Done))
        .map(|t| t.text.clone());
    let since = results
        .iter()
        .filter(|(i, id, _)| *i > at && !task_calls.contains(id))
        .count();
    Some(Checklist {
        done,
        total: args.tasks.len(),
        active,
        since,
    })
}

/// The reminder the model gets when its own checklist has gone stale
/// (issue #102), or `None` when there is nothing to remind it of.
///
/// `Some` when [`open_checklist`] finds at least one step that is not
/// `Done` and at least [`TASK_REMINDER_CALLS`] results have landed since.
/// No event is appended and nothing is cached: the caller puts the text at
/// the end of one model call's context and nowhere else.
pub fn stale_tasks_reminder(events: &[Event]) -> Option<String> {
    let Checklist {
        done, total, since, ..
    } = open_checklist(events)?;
    if done == total {
        return None;
    }
    (since >= TASK_REMINDER_CALLS).then(|| {
        format!(
            "Your checklist reads {done}/{total} done, last updated {since} tool calls ago. \
             If a step has started or finished since, call update_tasks now."
        )
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PinArgs {
    text: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LoadSkillArgs {
    name: String,
}

/// A `finish_step` call's arguments: the §4 field table, spelled out
/// rather than flattened, because serde ignores `deny_unknown_fields`
/// next to `flatten` and a typo the model sends must be named back, not
/// dropped. `deny` is not among them — the runner derives it from the
/// workflow, so a step never tells itself what to allow.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinishStepArgs {
    #[serde(default)]
    pub status: Option<ReportStatus>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub slots: Option<BTreeMap<String, serde_json::Value>>,
    #[serde(default)]
    pub planned_tests: Option<Vec<PlannedTest>>,
    #[serde(default)]
    pub commits: Option<Vec<CommitRef>>,
    #[serde(default)]
    pub ledger: Option<Vec<LedgerEntry>>,
    #[serde(default)]
    pub quotes: Option<Vec<String>>,
    #[serde(default)]
    pub verdict: Option<Verdict>,
    #[serde(default)]
    pub fix: Option<FixSize>,
    #[serde(default)]
    pub release_impact: Option<ReleaseImpact>,
    #[serde(default)]
    pub findings: Option<Vec<Finding>>,
    #[serde(default)]
    pub route: Option<Route>,
    #[serde(default)]
    pub handoff: Option<Handoff>,
}

impl From<FinishStepArgs> for StepReport {
    fn from(args: FinishStepArgs) -> Self {
        StepReport {
            status: args.status,
            body: args.body,
            slots: args.slots,
            planned_tests: args.planned_tests,
            commits: args.commits,
            ledger: args.ledger,
            quotes: args.quotes,
            verdict: args.verdict,
            fix: args.fix,
            release_impact: args.release_impact,
            findings: args.findings,
            route: args.route,
            handoff: args.handoff,
        }
    }
}

/// The harness tools' specs, in name order, with no `read_brief`: the
/// shape every caller that has no reach rows (a library, a test) wants.
pub fn harness_specs(
    offer_load_skill: bool,
    offer_finish_step: bool,
    offer_suggest_project: bool,
) -> Vec<ToolSpec> {
    harness_specs_with(
        offer_load_skill,
        offer_finish_step,
        offer_suggest_project,
        false,
    )
}

/// The same, with `read_brief` offered when a sibling in reach carries a
/// brief (issue #123). `Runtime::tool_specs` passes what `read_brief_offered`
/// decides; the three-argument `harness_specs` is this with `false`.
pub fn harness_specs_with(
    offer_load_skill: bool,
    offer_finish_step: bool,
    offer_suggest_project: bool,
    offer_read_brief: bool,
) -> Vec<ToolSpec> {
    let mut specs = vec![
        ToolSpec {
            name: ASK_HUMAN.into(),
            description: "Ask the human and wait for their answer. Use it when you cannot proceed without a decision only they can make. Put every question in the call (1-4, never \"answer the questions above\"), each with options when the answer is a choice; the client adds an \"Other: type your own\" row, so never list one yourself.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "question": {"type": "string", "description": "One question (the old shape)."},
                    "questions": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 4,
                        "description": "Every question in the call, 1-4.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "question": {"type": "string", "description": "The question, in full."},
                                "header": {"type": "string", "description": "A short name for the answer line, e.g. 'colour'."},
                                "options": {
                                    "type": "array",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": {"type": "string", "description": "What picking this option answers."},
                                            "description": {"type": "string", "description": "One line on what it means."}
                                        },
                                        "required": ["label"]
                                    }
                                },
                                "multi": {"type": "boolean", "description": "Allow picking several options."}
                            },
                            "required": ["question"]
                        }
                    }
                }
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
        ToolSpec {
            name: RECALL.into(),
            description: RECALL_DESCRIPTION.into(),
            // Exactly one form per call, so the schema is the three forms
            // and nothing else: each is closed, and their union is the
            // whole vocabulary. The arguments are still checked by name
            // (`recall_form`), since a provider may ignore `anyOf`.
            schema: json!({
                "type": "object",
                "anyOf": [
                    {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "handle": {"type": "integer", "description": "The seq of a dropped result, as its stub or truncation marker names it."},
                            "from_line": {"type": "integer", "description": "The first line to show (1-based), for paging through a long result."}
                        },
                        "required": ["handle"]
                    },
                    {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "from": {"type": "integer", "description": "The first seq to show."},
                            "to": {"type": "integer", "description": "The last seq to show, inclusive."}
                        },
                        "required": ["from", "to"]
                    },
                    {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "query": {"type": "string", "description": "Text to search the whole thread for."},
                            "limit": {"type": "integer", "description": "How many hits to show, 10 by default, 50 at most."}
                        },
                        "required": ["query"]
                    }
                ]
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
    if offer_finish_step {
        specs.push(ToolSpec {
            name: FINISH_STEP.into(),
            description: "Report the step's §4 fields and end the turn. Fill every field your role's template names: the runner reads the report, not the thread. `status: partial` needs `handoff`. `body` is the report's prose, which the runner posts under the step's heading: put it in `body`, never inside `slots`. The runtime writes the report as a `step_reported` event and ends the turn with `step_reported`; a second call in the same turn is refused.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "status": {"type": "string", "enum": ["done", "partial"], "description": "`partial` needs `handoff`."},
                    "body": {"type": "string", "description": "The report's prose, which the runner posts; required, and never inside `slots`."},
                    "slots": {"type": "object", "description": "Rendered template slots, by name."},
                    "planned_tests": {"type": "array", "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string"},
                            "what": {"type": "string"},
                            "derivation": {"type": "string", "description": "How the expected value is derived, not a hand-computed literal."}
                        },
                        "required": ["id", "what", "derivation"]
                    }},
                    "commits": {"type": "array", "items": {
                        "type": "object",
                        "properties": {"sha": {"type": "string"}, "subject": {"type": "string"}},
                        "required": ["sha", "subject"]
                    }},
                    "ledger": {"type": "array", "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string"},
                            "name": {"type": "string", "description": "The test's name in the code."},
                            "status": {"type": "string", "enum": ["landed", "not_landed"]},
                            "reason": {"type": "string", "description": "Why, when it did not land."}
                        },
                        "required": ["id", "name", "status"]
                    }},
                    "quotes": {"type": "array", "items": {"type": "string"}, "description": "Phrases the report attributes to the plan, verbatim."},
                    "verdict": {"type": "string", "enum": ["approve", "changes_needed"]},
                    "fix": {"type": "string", "enum": ["none", "trivial", "full"]},
                    "release_impact": {"type": "string", "enum": ["none", "patch", "minor", "breaking"]},
                    "findings": {"type": "array", "items": {
                        "type": "object",
                        "properties": {"id": {"type": "string"}, "text": {"type": "string"}},
                        "required": ["id", "text"]
                    }},
                    "route": {"type": "object", "properties": {"escalate": {
                        "type": "object",
                        "properties": {"reason": {"type": "string"}},
                        "required": ["reason"]
                    }}},
                    "handoff": {"type": "object", "properties": {
                        "done": {"type": "string"},
                        "next": {"type": "string"},
                        "dirty": {"type": "array", "items": {"type": "string"}}
                    }, "required": ["done", "next"]}
                },
                "required": ["status", "body"]
            }),
        });
    }
    if offer_suggest_project {
        specs.push(ToolSpec {
            name: SUGGEST_PROJECT.into(),
            description: SUGGEST_PROJECT_DESCRIPTION.into(),
            schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "project": {"type": "string", "description": "The project to switch to, by name, as the system prompt lists it."},
                    "reason": {"type": "string", "description": "One line on why the message belongs there."}
                },
                "required": ["project", "reason"]
            }),
        });
    }
    if offer_read_brief {
        specs.push(ToolSpec {
            name: READ_BRIEF.into(),
            description: READ_BRIEF_DESCRIPTION.into(),
            schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "project": {"type": "string", "description": "A project in reach, by the name the system prompt lists it under."}
                },
                "required": ["project"]
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
        READ_BRIEF.into(),
        RECALL.into(),
        SUGGEST_PROJECT.into(),
        UPDATE_TASKS.into(),
    ]
}

/// Whether the runtime answers this tool itself. `finish_step` is in the
/// list so the arm can refuse it honestly in an ordinary thread; it is
/// not in `harness_names`, which says what is always available.
pub fn is_harness_tool(name: &str) -> bool {
    matches!(
        name,
        PIN | ASK_HUMAN
            | LOAD_SKILL
            | READ_BRIEF
            | RECALL
            | UPDATE_TASKS
            | FINISH_STEP
            | SUGGEST_PROJECT
    )
}

/// A `recall` call's form: its arguments parsed, then exactly one of the
/// three forms (issue #75). An unknown key is `invalid arguments: …`, as
/// for every other harness call; any other combination is the form error.
fn recall_form(args: &serde_json::Value) -> Result<crate::recall_tool::Form, String> {
    match serde_json::from_value::<crate::recall_tool::RecallArgs>(args.clone()) {
        Ok(parsed) => crate::recall_tool::form(&parsed),
        Err(e) => Err(format!("invalid arguments: {e}")),
    }
}

/// Every harness tool is `safe`.
pub const HARNESS_CLASS: RiskClass = RiskClass::Safe;

/// A proposal raised while no turn runs (issue #85), as #92's start-up
/// path hands it back: the `call_id` its answer will carry, and the
/// `decision_proposed` event's id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdleProposal {
    pub call_id: String,
    pub proposal: ulid::Ulid,
}

/// What an answer settled (issues #7, #85): the log writes are already
/// done, and this says which they were, so `apply_switch` can render the
/// model-facing text without repeating the `match`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settled {
    /// A `yes` that moved the thread.
    Switched,
    /// A `no`: the thread stays where it is.
    Declined,
    /// A correction, which is the person's own words.
    Corrected(String),
    /// The answer was a withdrawal.
    Withdrawn(String),
    /// A `yes` whose `set_project` failed: the note is `switch_failed(e)`.
    SwitchFailed(String),
    /// A `yes` with no context to switch to: the note is
    /// `switch_failed(NO_CONTEXT)`.
    NoContext(String),
}

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
                Ok(args) => {
                    let question = plain_question(&args.questions);
                    match self.decisions.clone() {
                        Some(decisions) => {
                            let pending = crate::Pending::Human {
                                call_id: call.id.clone(),
                                question: question.clone(),
                                questions: args.questions.clone(),
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
                        None => match self.approver.ask_human(&question) {
                            Some(answer) => {
                                by = self.approver.author();
                                ok(answer)
                            }
                            None => err("no human available; treat this as a no".into()),
                        },
                    }
                }
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
                    Some(m) => {
                        // The folder is what makes a skill's own cited
                        // files readable: the boundary has it as a root,
                        // so the model can hand `read_file` the absolute
                        // path it is shown here (issue #124).
                        let folder = aigentic_policy::canonicalise(&m.path);
                        self.append_skill_loaded(&args.name, Invoker::Model, observe)?;
                        ok(format!(
                            "loaded skill `{}`; its instructions are now in context\n\
                             files beside this skill are in {}",
                            args.name,
                            folder.display()
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
            // A step thread's report (issue #55). The event is written
            // before the turn ends, so the runner's `step_finished` has
            // it; a call in an ordinary thread, or a second call in one
            // turn, is refused with no event.
            FINISH_STEP => {
                let Some(step) = self.step().map(str::to_owned) else {
                    return Ok((
                        err("finish_step is only offered to a step thread".into()),
                        by,
                    ));
                };
                if self.reported_this_turn() {
                    return Ok((
                        err("finish_step already ran; the report was recorded".into()),
                        by,
                    ));
                }
                let reported = format!("step reported for {step}; this ends the turn");
                match serde_json::from_value::<FinishStepArgs>(call.args.clone()) {
                    Err(e) => err(format!("invalid arguments: {e}")),
                    Ok(args) if args.status.is_none() => {
                        err("status is required: done | partial (partial needs handoff)".into())
                    }
                    // The runner posts the body and nothing else, so a report
                    // without one is refused here, while the turn can fix it.
                    Ok(args) if args.body.as_deref().is_none_or(|b| b.trim().is_empty()) => {
                        err(BODY_REQUIRED.into())
                    }
                    Ok(args) if args.status == Some(ReportStatus::Partial) => {
                        if args.handoff.is_none() {
                            err("a partial report must carry handoff { done, next, dirty }".into())
                        } else {
                            self.append_step_reported(args, observe)?;
                            ok(reported)
                        }
                    }
                    Ok(args) => {
                        self.append_step_reported(args, observe)?;
                        ok(reported)
                    }
                }
            }
            RECALL => match recall_form(&call.args) {
                Err(e) => err(e),
                Ok(form) => match crate::recall_tool::output(self.log.events(), &form) {
                    Ok(text) => ok(text),
                    Err(e) => err(e),
                },
            },
            // A sibling's brief, read-only (issue #123): the name is
            // matched against the reach rows and the file is opened at
            // the matched row's own path, never at a path the model
            // wrote.
            READ_BRIEF => match serde_json::from_value::<ReadBriefArgs>(call.args.clone()) {
                Err(e) => err(format!("invalid arguments: {e}")),
                Ok(args) => match self.read_brief(&args.project) {
                    Ok(text) => ok(text),
                    Err(message) => err(message),
                },
            },
            // The project proposal (issue #7). A step thread refuses it
            // outright, as `set_project` would; a proposal naming the
            // project the thread is already in never becomes a
            // decision.
            SUGGEST_PROJECT => match suggest_project_args(&call.args) {
                Err(e) => err(e),
                Ok(args) => {
                    // A step thread never sees the spec, but a call still
                    // executes: it parks as anywhere else, and the `yes`
                    // that follows is where `set_project` refuses it (T4).
                    if self.current_project().as_deref() == Some(args.project.as_str()) {
                        err(already_in_text(&args.project))
                    } else {
                        let (result, who) =
                            self.suggest_project(call, args, cancel, observe).await?;
                        by = who;
                        result
                    }
                }
            },
            other => err(format!("unknown harness tool: {other}")),
        };
        Ok((result, by))
    }

    /// Ask for a switch (issue #7). The `decision_proposed` event is
    /// written before the wait is registered, so a client that sees the
    /// waiting signal already finds it in the log; a `yes` is the one
    /// answer that moves the thread, and it is recorded before it moves.
    async fn suggest_project(
        &mut self,
        call: &ToolCall,
        args: SuggestProjectArgs,
        cancel: &crate::CancelToken,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<(ToolResult, Author), RuntimeError> {
        let id = call.id.clone();
        let payload = DecisionProposedPayload {
            kind: DecisionKind::Project,
            proposal: proposal_text(&args.project),
            target: Some(args.project.clone()),
            reason: args.reason.clone(),
            call_id: Some(id.clone()),
            stage: DecisionStage::Ask,
        };
        let proposal = self.append(
            EventKind::DecisionProposed,
            Author::Agent(self.agent.clone()),
            serde_json::to_value(payload).expect("serialisable"),
            None,
            observe,
        )?;
        let staying = self.current_project();
        match self.decisions.clone() {
            None => {
                // A bare runtime has nobody to ask, so the proposal is
                // withdrawn rather than left open: it is still written,
                // so the fold pairs the answer with it.
                let note = NO_ONE_TO_ANSWER.to_owned();
                self.withdraw_switch(proposal.id, note.clone(), observe)?;
                Ok((refused(&id, &not_answered_text(&note)), Author::System))
            }
            Some(decisions) => {
                let pending = crate::Pending::Switch {
                    call_id: id.clone(),
                    project: args.project.clone(),
                    reason: args.reason.clone(),
                };
                let rx = decisions.register(pending.clone());
                observe(Signal::Waiting(&pending));
                tokio::select! {
                    biased;
                    _by = cancel.cancelled() => {
                        decisions.withdraw(&id);
                        let note = TURN_INTERRUPTED.to_owned();
                        self.withdraw_switch(proposal.id, note.clone(), observe)?;
                        Ok((refused(&id, &not_answered_text(&note)), Author::System))
                    }
                    decided = rx => match decided {
                        Ok(crate::Answered::Switch { answer, by: who, ctx }) => {
                            self.apply_switch(id, args, proposal.id, answer, who, ctx, staying, observe).await
                        }
                        // The table dropped the sender without an answer:
                        // no path does, but the thread must not move and
                        // must not be left waiting, so it reads as
                        // withdrawn.
                        _ => {
                            let note = NO_ONE_TO_ANSWER.to_owned();
                            self.withdraw_switch(proposal.id, note.clone(), observe)?;
                            Ok((refused(&id, &not_answered_text(&note)), Author::System))
                        }
                    },
                }
            }
        }
    }

    /// Apply a person's answer to a proposal (issue #7): record it, and
    /// for a `yes` switch the project first, then say so. The ack the
    /// answer came with fires with the outcome, so the session that sent
    /// the context learns whether it was used.
    #[allow(clippy::too_many_arguments)]
    async fn apply_switch(
        &mut self,
        id: String,
        args: SuggestProjectArgs,
        proposal: ulid::Ulid,
        answer: crate::SwitchAnswer,
        who: Author,
        ctx: crate::SwitchCtx,
        staying: Option<String>,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<(ToolResult, Author), RuntimeError> {
        let settled = self
            .settle_switch(proposal, answer, who.clone(), ctx, observe)
            .await?;
        // The model-facing text, from what was settled: `Withdrawn` and a
        // failed `yes` are the same refusal with the note spelled out; a
        // `yes` with no context to switch to is the bare note, as it has
        // been since #7.
        let result = match &settled {
            Settled::Switched => said(&id, switched_text(&args.project)),
            Settled::Declined => said(
                &id,
                match staying.as_deref() {
                    Some(project) => declined_text(project),
                    // A thread in no project has no name to stay in.
                    None => DECLINED_NO_PROJECT.to_owned(),
                },
            ),
            Settled::Corrected(where_it_belongs) => said(&id, corrected_text(where_it_belongs)),
            Settled::Withdrawn(note) | Settled::SwitchFailed(note) => {
                refused(&id, &not_answered_text(note))
            }
            Settled::NoContext(note) => refused(&id, note),
        };
        // An answer a person gave is theirs; a refused or withdrawn one
        // is the system's.
        let by = match settled {
            Settled::Switched | Settled::Declined | Settled::Corrected(_) => who,
            Settled::Withdrawn(_) | Settled::SwitchFailed(_) | Settled::NoContext(_) => {
                Author::System
            }
        };
        Ok((result, by))
    }

    /// The core of an answer, whatever raised the proposal (issues #7,
    /// #85): record it, and for a `yes` switch the project first, then
    /// say so. The ack the answer came with fires with the outcome, so
    /// the session that sent the context learns whether it was used.
    ///
    /// `apply_switch` turns what this settles into the model-facing text;
    /// `answer_switch_idle` is this and nothing else, so a start-up
    /// proposal's `yes`, failure and ack behave exactly as #7's.
    async fn settle_switch(
        &mut self,
        proposal: ulid::Ulid,
        answer: crate::SwitchAnswer,
        who: Author,
        ctx: crate::SwitchCtx,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<Settled, RuntimeError> {
        match answer {
            crate::SwitchAnswer::Yes => match ctx.take() {
                Some((target, ack)) => {
                    // The switch happens here, in the turn, and the
                    // project_switched event lands before the human's
                    // answer does: the log says which project the yes
                    // was about by naming the switch first.
                    match self.set_project(target, who.clone(), observe) {
                        Ok(()) => {
                            self.append_decision_answered(
                                DecisionAnswer::Yes,
                                None,
                                None,
                                who,
                                proposal,
                                observe,
                            )?;
                            let _ = ack.send(Ok(()));
                            Ok(Settled::Switched)
                        }
                        Err(e) => {
                            let note = switch_failed(&e.to_string());
                            self.withdraw_switch(proposal, note.clone(), observe)?;
                            // The session that built the context is
                            // told why it was not used.
                            let _ = ack.send(Err(note.clone()));
                            Ok(Settled::SwitchFailed(note))
                        }
                    }
                }
                None => {
                    // A `yes` with no context to switch to: nothing can
                    // move, so nothing moves, and the answer is recorded
                    // as withdrawn. Never a `yes` for a switch that did
                    // not happen.
                    let note = switch_failed(NO_CONTEXT);
                    self.withdraw_switch(proposal, note.clone(), observe)?;
                    Ok(Settled::NoContext(note))
                }
            },
            crate::SwitchAnswer::No => {
                self.append_decision_answered(
                    DecisionAnswer::No,
                    None,
                    None,
                    who,
                    proposal,
                    observe,
                )?;
                Ok(Settled::Declined)
            }
            crate::SwitchAnswer::Corrected(where_it_belongs) => {
                self.append_decision_answered(
                    DecisionAnswer::Corrected,
                    Some(where_it_belongs.clone()),
                    None,
                    who,
                    proposal,
                    observe,
                )?;
                Ok(Settled::Corrected(where_it_belongs))
            }
            crate::SwitchAnswer::Withdrawn(note) => {
                self.withdraw_switch(proposal, note.clone(), observe)?;
                Ok(Settled::Withdrawn(note))
            }
        }
    }

    /// Propose a switch while no turn runs (issue #85): #92's start-up
    /// proposal, for a person who opened their front thread from another
    /// project's folder.
    ///
    /// The event is an ordinary `decision_proposed` with
    /// `Author::System`, and its `call_id` starts with
    /// [`aigentic_log::STARTUP_PREFIX`], which is what `stats` and
    /// `declined_at_startup` read to tell it from the model's own. It is
    /// registered with nothing: no turn waits, so there is nothing to
    /// resume. The caller checks the thread is idle; this one does not.
    pub async fn propose_switch_idle(
        &mut self,
        project: &str,
        reason: &str,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<IdleProposal, RuntimeError> {
        // A step thread keeps its overlay for its whole life (issue #55),
        // so it has no project of its own to propose leaving.
        if let Some(step) = &self.step {
            return Err(RuntimeError::StepThread(format!(
                "`{}` is a step thread and cannot propose a switch",
                step.name
            )));
        }
        let call_id = format!("{}{}", aigentic_log::STARTUP_PREFIX, ulid::Ulid::generate());
        let payload = DecisionProposedPayload {
            kind: DecisionKind::Project,
            proposal: proposal_text(project),
            target: Some(project.to_owned()),
            reason: reason.to_owned(),
            call_id: Some(call_id.clone()),
            stage: DecisionStage::Ask,
        };
        let proposal = self.append(
            EventKind::DecisionProposed,
            Author::System,
            serde_json::to_value(payload).expect("serialisable"),
            None,
            observe,
        )?;
        Ok(IdleProposal {
            call_id,
            proposal: proposal.id,
        })
    }

    /// Answer an idle proposal (issue #85): `settle_switch` and nothing
    /// else, so the `yes` path, the failure path and the ack behave as
    /// #7's.
    ///
    /// A `decision_answered` is only ever written once per proposal:
    /// [`RuntimeError::NoOpenProposal`], writing nothing, when `proposal`
    /// names no `decision_proposed` in this log or already has an answer.
    /// The turn path gets that guard from the `Decisions` table
    /// ([`crate::DecisionError::AlreadyDecided`]); with no table entry
    /// this path asks the log instead, so a second answer cannot land as
    /// an orphan in `decision_records`.
    pub async fn answer_switch_idle(
        &mut self,
        proposal: ulid::Ulid,
        answer: crate::SwitchAnswer,
        who: Author,
        ctx: crate::SwitchCtx,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<Settled, RuntimeError> {
        let open = {
            let events = self.log.events();
            let raised = events
                .iter()
                .any(|e| e.id == proposal && e.kind == EventKind::DecisionProposed);
            let answered = events
                .iter()
                .any(|e| e.kind == EventKind::DecisionAnswered && e.parent_event == Some(proposal));
            raised && !answered
        };
        if !open {
            return Err(RuntimeError::NoOpenProposal(proposal.to_string()));
        }
        self.settle_switch(proposal, answer, who, ctx, observe)
            .await
    }

    /// Record a proposal nobody answered as `withdrawn`, by the system.
    fn withdraw_switch(
        &mut self,
        proposal: ulid::Ulid,
        note: String,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<(), RuntimeError> {
        self.append_decision_answered(
            DecisionAnswer::Withdrawn,
            None,
            Some(note),
            Author::System,
            proposal,
            observe,
        )
    }

    /// Append `step_reported` for a step's report (issue #55). The author
    /// is the agent — the report is the step's own. Written before the
    /// turn ends, so a `step_finished` that follows can name it.
    fn append_step_reported(
        &mut self,
        args: FinishStepArgs,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<(), RuntimeError> {
        let report: StepReport = args.into();
        let payload = serde_json::to_value(&report).expect("serialisable");
        self.append(
            EventKind::StepReported,
            Author::Agent(self.agent.clone()),
            payload,
            None,
            observe,
        )?;
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // The runtime parses each harness call with
    // `serde_json::from_value::<T>(call.args.clone())`, so these go
    // through the same path.

    // --- The stale checklist reminder (issue #102) ---------------------
    //
    // Hand-built logs, because the function is pure: it reads the open
    // turn's `update_tasks` calls and the results that answer them.

    use aigentic_log::{PolicyRecord, TurnEndedPayload};
    use time::OffsetDateTime;
    use ulid::Ulid;

    /// A `K - 1`/`K` boundary test needs the constant by hand.
    const K: usize = TASK_REMINDER_CALLS;

    /// The reminder as the issue spells it, from a fixture's counts.
    fn reminder(done: usize, total: usize, since: usize) -> String {
        format!(
            "Your checklist reads {done}/{total} done, last updated {since} tool calls ago. If a \
             step has started or finished since, call update_tasks now."
        )
    }

    fn event(seq: u64, kind: EventKind, payload: serde_json::Value) -> Event {
        Event {
            id: Ulid::from_parts(1_700_000_000_000 + seq, u128::from(seq)),
            thread_id: Ulid::from_parts(1_700_000_000_000, 7),
            seq,
            kind,
            author: Author::System,
            payload,
            parent_event: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    /// An assistant message carrying one or more `update_tasks` calls.
    fn push_tasks(events: &mut Vec<Event>, calls: Vec<(&str, serde_json::Value)>) {
        let blocks = calls
            .into_iter()
            .map(|(id, args)| {
                ContentBlock::ToolCall(ToolCall {
                    id: id.into(),
                    name: UPDATE_TASKS.into(),
                    args,
                })
            })
            .collect();
        let payload = serde_json::to_value(AssistantMessagePayload {
            blocks,
            usage: None,
            finish_reason: None,
        })
        .unwrap();
        let seq = events.len() as u64;
        events.push(event(seq, EventKind::AssistantMessage, payload));
    }

    /// A tool result answering the call `id`.
    fn push_result(events: &mut Vec<Event>, id: &str, is_error: bool) {
        let result = ToolResult {
            id: id.into(),
            content: if is_error {
                "invalid arguments".into()
            } else {
                "ok".into()
            },
            is_error,
        };
        let payload = serde_json::to_value(ToolResultPayload::new(
            result,
            PolicyRecord::rule("test", "allow"),
        ))
        .unwrap();
        let seq = events.len() as u64;
        events.push(event(seq, EventKind::ToolResult, payload));
    }

    /// `n` successful results of other tools, ids from `first`.
    fn push_others(events: &mut Vec<Event>, first: usize, n: usize) {
        for k in 0..n {
            push_result(events, &format!("call_{}", first + k), false);
        }
    }

    fn push_turn_ended(events: &mut Vec<Event>) {
        let payload = serde_json::to_value(TurnEndedPayload::new("done")).unwrap();
        let seq = events.len() as u64;
        events.push(event(seq, EventKind::TurnEnded, payload));
    }

    /// A checklist of `total` steps, the first `done` of them done.
    fn tasks_json(done: usize, total: usize) -> serde_json::Value {
        let tasks: Vec<serde_json::Value> = (0..total)
            .map(|i| {
                json!({
                    "text": format!("step {i}"),
                    "state": if i < done { "done" } else { "pending" },
                })
            })
            .collect();
        json!({"tasks": tasks})
    }

    /// T1: the wording the issue fixes, filled from the fixture's counts.
    #[test]
    fn the_reminder_text_is_the_issue_text_with_the_fixtures_counts() {
        let mut events = Vec::new();
        push_tasks(&mut events, vec![("u1", tasks_json(2, 3))]);
        push_result(&mut events, "u1", false);
        push_others(&mut events, 0, K);

        let text = stale_tasks_reminder(&events).expect("a reminder");
        assert_eq!(
            text,
            format!(
                "Your checklist reads 2/3 done, last updated {K} tool calls ago. If a step has \
                 started or finished since, call update_tasks now."
            )
        );
    }

    /// T1: no checklist in the open turn, however busy the turn was.
    #[test]
    fn no_update_tasks_in_the_open_turn_gives_no_reminder() {
        let mut events = Vec::new();
        push_others(&mut events, 0, K + 2);
        assert_eq!(stale_tasks_reminder(&events), None);
    }

    /// T1: an all-done checklist is not stale, whatever `since` says.
    #[test]
    fn an_all_done_checklist_gives_no_reminder_however_many_calls_since() {
        let mut events = Vec::new();
        push_tasks(&mut events, vec![("u1", tasks_json(3, 3))]);
        push_result(&mut events, "u1", false);
        push_others(&mut events, 0, K);
        assert_eq!(stale_tasks_reminder(&events), None);
        push_others(&mut events, 100, K);
        assert_eq!(stale_tasks_reminder(&events), None);
    }

    /// T1: `K - 1` results since the update are silent; the `K`-th speaks.
    #[test]
    fn the_reminder_waits_for_k_calls_since_the_update() {
        let mut events = Vec::new();
        push_tasks(&mut events, vec![("u1", tasks_json(1, 3))]);
        push_result(&mut events, "u1", false);
        push_others(&mut events, 0, K - 1);
        assert_eq!(stale_tasks_reminder(&events), None, "one call short");
        push_others(&mut events, 100, 1);
        assert_eq!(stale_tasks_reminder(&events), Some(reminder(1, 3, K)));
    }

    /// T1: a later successful update is the checklist, and resets `since`.
    #[test]
    fn a_later_successful_update_tasks_resets_the_count() {
        let mut events = Vec::new();
        push_tasks(&mut events, vec![("u1", tasks_json(1, 3))]);
        push_result(&mut events, "u1", false);
        push_others(&mut events, 0, K);
        assert!(stale_tasks_reminder(&events).is_some(), "stale by now");

        push_tasks(&mut events, vec![("u2", tasks_json(2, 3))]);
        push_result(&mut events, "u2", false);
        push_others(&mut events, 100, K - 1);
        assert_eq!(
            stale_tasks_reminder(&events),
            None,
            "counted from u2's result"
        );
        push_others(&mut events, 200, 1);
        assert_eq!(stale_tasks_reminder(&events), Some(reminder(2, 3, K)));
    }

    /// T1: a failed later update neither replaces the checklist nor counts.
    /// The refusal is one that parses (an empty list, which the handler
    /// refuses with "send at least one task"), so the rule under test is
    /// the `is_error` check, not the parser's `deny_unknown_fields`.
    #[test]
    fn a_later_failed_update_tasks_resets_nothing_and_counts_nothing() {
        let mut events = Vec::new();
        push_tasks(&mut events, vec![("u1", tasks_json(1, 3))]);
        push_result(&mut events, "u1", false);
        push_others(&mut events, 0, K);

        push_tasks(&mut events, vec![("u2", json!({"tasks": []}))]);
        push_result(&mut events, "u2", true);
        assert_eq!(
            stale_tasks_reminder(&events),
            Some(reminder(1, 3, K)),
            "the refusal is not a call: `since` stays K"
        );
    }

    /// T1: one message with two calls — the failed sibling is no checklist
    /// and its result is no call. The sibling is a parseable refusal (an
    /// empty list), as in the test above.
    #[test]
    fn a_failed_sibling_call_is_neither_the_checklist_nor_progress() {
        let mut events = Vec::new();
        push_tasks(
            &mut events,
            vec![("u1", tasks_json(1, 3)), ("u2", json!({"tasks": []}))],
        );
        push_result(&mut events, "u1", false);
        push_result(&mut events, "u2", true);
        push_others(&mut events, 0, K - 1);
        assert_eq!(
            stale_tasks_reminder(&events),
            None,
            "the sibling's result does not count towards `since`"
        );
        push_others(&mut events, 100, 1);
        assert_eq!(
            stale_tasks_reminder(&events),
            Some(reminder(1, 3, K)),
            "the first call's list is the checklist"
        );
    }

    /// T1: a checklist belongs to the turn that set it.
    #[test]
    fn a_checklist_from_a_previous_turn_gives_no_reminder() {
        let mut events = Vec::new();
        push_tasks(&mut events, vec![("u1", tasks_json(1, 3))]);
        push_result(&mut events, "u1", false);
        push_others(&mut events, 0, K);
        push_turn_ended(&mut events);
        push_others(&mut events, 100, K + 1);
        assert_eq!(stale_tasks_reminder(&events), None);
    }

    // --- `open_checklist` (issue #109) ---------------------------------
    //
    // #102's T1 cases, restated against the function the reminder now
    // calls: every expected count comes from the fixture's own list.

    /// A list whose states are spelled out, so a fixture can hold an
    /// `Active` step.
    fn tasks_with(states: &[&str]) -> serde_json::Value {
        let tasks: Vec<serde_json::Value> = states
            .iter()
            .enumerate()
            .map(|(i, state)| json!({"text": format!("step {i}"), "state": state}))
            .collect();
        json!({"tasks": tasks})
    }

    /// The `(done, total, active)` a fixture's own `update_tasks`
    /// arguments imply, counted rather than hand-written.
    fn from_list(args: &serde_json::Value) -> (usize, usize, Option<String>) {
        let tasks = args["tasks"].as_array().expect("tasks");
        let done = tasks.iter().filter(|t| t["state"] == "done").count();
        let active = tasks
            .iter()
            .find(|t| t["state"] == "active")
            .or_else(|| tasks.iter().find(|t| t["state"] != "done"))
            .map(|t| t["text"].as_str().expect("text").to_string());
        (done, tasks.len(), active)
    }

    /// T1: no list in the open turn, however many other calls landed.
    #[test]
    fn open_checklist_is_none_without_a_successful_update_tasks() {
        let mut events = Vec::new();
        push_others(&mut events, 0, K + 2);
        assert_eq!(open_checklist(&events), None);
    }

    /// T1: the open turn's counts, its active step, and the calls since.
    #[test]
    fn open_checklist_reports_the_lists_counts_and_the_calls_since() {
        let list = tasks_with(&["done", "active", "pending"]);
        let mut events = Vec::new();
        push_tasks(&mut events, vec![("u1", list.clone())]);
        push_result(&mut events, "u1", false);
        push_others(&mut events, 0, 3);
        let (done, total, active) = from_list(&list);
        assert_eq!(
            open_checklist(&events),
            Some(Checklist {
                done,
                total,
                active,
                since: 3,
            })
        );
    }

    /// T1: a step that is `Active` wins over an earlier unfinished one.
    #[test]
    fn open_checklist_prefers_the_active_step_to_the_first_unfinished_one() {
        let list = tasks_with(&["pending", "active", "pending"]);
        let mut events = Vec::new();
        push_tasks(&mut events, vec![("u1", list.clone())]);
        push_result(&mut events, "u1", false);
        let (done, total, active) = from_list(&list);
        assert_eq!(
            open_checklist(&events),
            Some(Checklist {
                done,
                total,
                active,
                since: 0,
            }),
            "no calls have landed since the list"
        );
    }

    /// T1: an all-done list is still a checklist; `active` is then none.
    #[test]
    fn open_checklist_keeps_an_all_done_list_with_no_active_step() {
        let list = tasks_with(&["done", "done"]);
        let mut events = Vec::new();
        push_tasks(&mut events, vec![("u1", list.clone())]);
        push_result(&mut events, "u1", false);
        push_others(&mut events, 0, K);
        let (done, total, active) = from_list(&list);
        assert_eq!(
            open_checklist(&events),
            Some(Checklist {
                done,
                total,
                active,
                since: K,
            })
        );
    }

    /// T1: a later successful list is the checklist, and resets `since`.
    #[test]
    fn open_checklist_takes_the_later_successful_list() {
        let first = tasks_json(1, 3);
        let second = tasks_json(2, 3);
        let mut events = Vec::new();
        push_tasks(&mut events, vec![("u1", first)]);
        push_result(&mut events, "u1", false);
        push_others(&mut events, 0, K);
        push_tasks(&mut events, vec![("u2", second.clone())]);
        push_result(&mut events, "u2", false);
        push_others(&mut events, 100, 4);
        let (done, total, active) = from_list(&second);
        assert_eq!(
            open_checklist(&events),
            Some(Checklist {
                done,
                total,
                active,
                since: 4,
            }),
            "counted from u2's result"
        );
    }

    /// T1: a later refused call is no checklist, so the earlier list stands
    /// and the refusal is not progress either.
    #[test]
    fn open_checklist_ignores_a_later_failed_call() {
        let first = tasks_json(1, 3);
        let mut events = Vec::new();
        push_tasks(&mut events, vec![("u1", first.clone())]);
        push_result(&mut events, "u1", false);
        push_others(&mut events, 0, K);
        push_tasks(&mut events, vec![("u2", json!({"tasks": []}))]);
        push_result(&mut events, "u2", true);
        let (done, total, active) = from_list(&first);
        assert_eq!(
            open_checklist(&events),
            Some(Checklist {
                done,
                total,
                active,
                since: K,
            }),
            "the refusal is not a call: `since` stays K"
        );
    }

    /// T1: two calls in one message — the failed sibling is no checklist
    /// and its result is no progress.
    #[test]
    fn open_checklist_ignores_a_failed_sibling_call() {
        let good = tasks_json(1, 3);
        let mut events = Vec::new();
        push_tasks(
            &mut events,
            vec![("u1", good.clone()), ("u2", json!({"tasks": []}))],
        );
        push_result(&mut events, "u1", false);
        push_result(&mut events, "u2", true);
        push_others(&mut events, 0, 4);
        let (done, total, active) = from_list(&good);
        assert_eq!(
            open_checklist(&events),
            Some(Checklist {
                done,
                total,
                active,
                since: 4,
            }),
            "the sibling's failure is not a call"
        );
    }

    /// T1: the same list, once the turn has ended, is the previous turn's.
    #[test]
    fn open_checklist_is_none_after_the_turn_ended() {
        let mut events = Vec::new();
        push_tasks(&mut events, vec![("u1", tasks_json(1, 3))]);
        push_result(&mut events, "u1", false);
        push_others(&mut events, 0, K);
        push_turn_ended(&mut events);
        push_others(&mut events, 100, 2);
        assert_eq!(open_checklist(&events), None);
    }

    #[test]
    fn ask_human_rejects_unknown_argument_keys() {
        let err = serde_json::from_value::<AskHumanArgs>(json!({
            "questions": [{"question": "which?"}],
            "bogus": "leaked tool-call template",
        }))
        .unwrap_err();
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn task_rejects_unknown_argument_keys() {
        let err = serde_json::from_value::<Task>(json!({
            "text": "step",
            "bogus": "leaked tool-call template",
        }))
        .unwrap_err();
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn update_tasks_rejects_unknown_argument_keys() {
        let err = serde_json::from_value::<UpdateTasksArgs>(json!({
            "tasks": [{"text": "step"}],
            "bogus": "leaked tool-call template",
        }))
        .unwrap_err();
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn pin_rejects_unknown_argument_keys() {
        let err = serde_json::from_value::<PinArgs>(json!({
            "text": "a fact",
            "bogus": "leaked tool-call template",
        }))
        .unwrap_err();
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn load_skill_rejects_unknown_argument_keys() {
        let err = serde_json::from_value::<LoadSkillArgs>(json!({
            "name": "brief",
            "bogus": "leaked tool-call template",
        }))
        .unwrap_err();
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn human_question_rejects_unknown_argument_keys() {
        let err = serde_json::from_value::<HumanQuestion>(json!({
            "question": "which?",
            "bogus": "leaked tool-call template",
        }))
        .unwrap_err();
        assert!(err.to_string().contains("bogus"), "{err}");
    }
}
