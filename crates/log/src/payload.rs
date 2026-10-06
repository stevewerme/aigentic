//! Kind-specific payload shapes. `Event.payload` is free JSON on the wire;
//! these structs are the contract for what each kind carries.

use std::collections::BTreeMap;
use std::path::PathBuf;

use aigentic_core::{Author, ContentBlock, ProviderError, RiskClass, ToolCall, ToolResult};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

/// Payload of a `user_message` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserMessagePayload {
    pub blocks: Vec<ContentBlock>,
    /// Arrived while a turn was running (phase 5's queue). It is in the
    /// log at once so every subscriber sees it. Lines from before phase 5
    /// read back as `false`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mid_turn: bool,
    /// A mid-turn message that steers (issue #33): emitted where it sits
    /// in the log, so the very next model call sees it. Without it the
    /// horizon rule holds and the message waits for the next turn, which
    /// is what every log from before steering did. `false` by default, so
    /// old logs replay exactly as they ran.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub steer: bool,
}

impl UserMessagePayload {
    pub fn new(blocks: Vec<ContentBlock>) -> Self {
        Self {
            blocks,
            mid_turn: false,
            steer: false,
        }
    }
}

/// `project_switched`: the thread's project changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectSwitchedPayload {
    pub from: Option<String>,
    pub to: Option<String>,
    /// The new working root.
    pub root: std::path::PathBuf,
    /// The workspace the new project is in, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
}

/// `thread_renamed`: the thread's title.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadRenamedPayload {
    pub title: String,
    /// The provider that proposed the title, when a side job wrote this
    /// line: the utility profile's model name (issue #49). A person's
    /// `/rename` makes no call and carries neither this nor a usage, and
    /// neither did any line written before the runtime recorded them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// What that call cost in tokens, with `cost_usd` stamped from the
    /// utility profile's `[prices]` by the same rule as
    /// `memory_extracted` (issues #46, #49). Absent whenever no call
    /// stands behind the line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

/// Payload of a `thread_started` event, the first event of a thread the
/// daemon created: the project it belongs to (`None` outside any) and
/// the root its tools run in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadStartedPayload {
    pub project: Option<String>,
    pub root: PathBuf,
    pub created_by: Author,
    /// The runner's child threads (layer 2, issue #53): the lead thread
    /// this step was started from. `None` for a thread a person or the
    /// TUI started, and for every line written before the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_thread: Option<Ulid>,
    /// The workflow step this thread fills, set exactly when
    /// `parent_thread` is (a step's child has one; the lead thread does
    /// not). `None` on older lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    /// The person's front thread (#84): plain `aigentic` reopens the newest one they made.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub front: bool,
}

/// Token usage for one model call, as persisted. The token fields mirror
/// `aigentic_core::Usage`; older log lines without the cache fields read
/// back as zero.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: Option<u64>,
    /// `true` when the numbers came from `Provider::count_tokens` because
    /// the provider reported no usage. `/cost` shows that share separately.
    #[serde(default)]
    pub estimated: bool,
    /// The profile the call ran on (`/profile` can change it mid-thread).
    /// Absent on lines written before issue #31, so an old log replays
    /// and shows no price.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// The model name of that profile, for per-model totals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The reasoning effort that call ran at, as the profile names it
    /// (`50`, `high`), so output tokens and cost can be compared by
    /// effort (issue #44). Absent when the profile sets none, and on
    /// every older line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Wall time from the request to the turn's end for the call, and
    /// time to the first streamed block. Both absent on old lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    /// What the call cost, from the profile's `[prices]`, when the
    /// profile sets them: `None` otherwise (and on every old line), so a
    /// thread can be summed without a price table to look up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

impl Usage {
    /// Persist what the provider reported.
    pub fn reported(u: aigentic_core::Usage) -> Self {
        Self::from_core(u, false)
    }

    /// Persist an estimate made by the runtime.
    pub fn estimated(u: aigentic_core::Usage) -> Self {
        Self::from_core(u, true)
    }

    /// The same numbers as a core usage, for arithmetic that the
    /// harness (not the log) owns, such as the budget.
    pub fn to_core(&self) -> aigentic_core::Usage {
        aigentic_core::Usage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_read_tokens: self.cache_read_tokens,
            cache_write_tokens: self.cache_write_tokens,
            reasoning_tokens: self.reasoning_tokens,
        }
    }

    fn from_core(u: aigentic_core::Usage, estimated: bool) -> Self {
        Self {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cache_read_tokens: u.cache_read_tokens,
            cache_write_tokens: u.cache_write_tokens,
            reasoning_tokens: u.reasoning_tokens,
            estimated,
            profile: None,
            model: None,
            effort: None,
            latency_ms: None,
            ttft_ms: None,
            cost_usd: None,
        }
    }

    pub fn total(&self) -> u64 {
        self.input_tokens + self.cache_read_tokens + self.cache_write_tokens + self.output_tokens
    }
}

/// Payload of an `assistant_message` event. Tool calls live in `blocks`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessagePayload {
    pub blocks: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// The provider's own reason the reply ended (issue #96): `"stop"`,
    /// `"tool_calls"`, `"length"`, `"end_of_stream"`, and whatever else a
    /// backend sends. Absent on lines written before it was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

/// What let a tool call run (or refused it). Recorded on every
/// `tool_result` so the log can be audited: no result without a record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PolicyRecord {
    /// A policy rule decided; `rule` names it, e.g. `"class read"` or
    /// `"bash allow-pattern cargo test"`, and `decision` is `"allow"` or
    /// `"deny"`. `reason` is the rule's own text, carried on a denial so
    /// the model reads why; absent on lines written before phase 4 step 9.
    Rule {
        rule: String,
        decision: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// A human decided; `event` is the `permission_decided` event.
    Human { event: Ulid, allow: bool },
}

impl PolicyRecord {
    /// A rule record carrying the rule's reason text.
    pub fn rule_with_reason(
        rule: impl Into<String>,
        decision: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self::Rule {
            rule: rule.into(),
            decision: decision.into(),
            reason: Some(reason.into()),
        }
    }

    pub fn rule(rule: impl Into<String>, decision: impl Into<String>) -> Self {
        Self::Rule {
            reason: None,
            rule: rule.into(),
            decision: decision.into(),
        }
    }

    /// The record the resume path puts on its synthetic error results.
    pub fn synthetic() -> Self {
        Self::rule("resume", "synthetic")
    }
}

/// Payload of a `tool_result` event; `parent_event` points at the
/// `assistant_message` that made the call.
///
/// The result's fields are flattened, so the wire shape is the phase 0
/// one plus an optional `policy`; lines written before phase 3 read back
/// with `policy: None`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResultPayload {
    #[serde(flatten)]
    pub result: ToolResult,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<PolicyRecord>,
}

impl ToolResultPayload {
    pub fn new(result: ToolResult, policy: PolicyRecord) -> Self {
        Self {
            result,
            policy: Some(policy),
        }
    }
}

/// Payload of a `turn_ended` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnEndedPayload {
    /// `"done"`, `"resumed"`, `"asked_human"`, `"length"`, the budget
    /// that was hit, or `provider_error: ...`.
    pub reason: String,
    /// The `path` of every `write_file` and `edit_file` call in the turn
    /// that returned without error, in order, each once; so a stop on a
    /// budget can say which files it was in. Empty on lines written before
    /// phase 4 step 9.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub touched: Vec<String>,
    /// Wall time from the turn's start to its end, on `SystemTime`
    /// (issue #47): logged on every measured turn, so `stats` can sum a
    /// thread's wall time without reconstructing it from event
    /// timestamps (which count idle time between turns). Added, never
    /// changed: `None` on old lines and on turns the runtime did not
    /// measure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_secs: Option<u64>,
    /// Wall time minus running time for the turn (issue #47): above the
    /// threshold this is time the machine slept mid-turn, so a duration
    /// is not silently inflated by a nap. `None` on old lines and on a
    /// turn that did not sleep.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slept_secs: Option<u64>,
    /// The part of `slept_secs` spent parked on a human (issue #47): a
    /// permission prompt or an `ask_human`, which the keep-awake guard
    /// deliberately releases for. `Some` only when `slept_secs` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slept_awaiting_secs: Option<u64>,
    /// The daemon's keep-awake guard status during the turn (issue #47):
    /// `"on"`, `"off"`, or `"unavailable: <reason>"`. `None` on old
    /// lines and when no daemon set one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_awake: Option<String>,
    /// The structured provider error when the turn ended on one (issue
    /// #22): the machine shape the plain line is derived from. `None`
    /// on old lines, on turns that did not fail on a provider, and when
    /// a shape this binary cannot read is stored — the field is read
    /// leniently (`de_lenient`), so a `turn_ended` event from another
    /// version always still parses and falls back to its raw `reason`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_lenient"
    )]
    pub error: Option<ProviderError>,
}

/// Read `Option<ProviderError>` without ever failing the whole payload
/// (issue #22): a `turn_ended` line whose `error` holds a variant,
/// rename or shape this binary does not know degrades to `None`, so the
/// line still replays through its raw `reason`.
fn de_lenient<'de, D>(d: D) -> Result<Option<ProviderError>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<ProviderError>::deserialize(d).unwrap_or(None))
}

impl TurnEndedPayload {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            touched: Vec::new(),
            wall_secs: None,
            slept_secs: None,
            slept_awaiting_secs: None,
            keep_awake: None,
            error: None,
        }
    }
}

/// What a `compacted` event does to its range in the projection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CompactionStrategy {
    /// Tool results in the range are shortened to head and tail. Reclaims
    /// most of a full context and costs no model call.
    TruncateResults { max_bytes: usize },
    /// Events in the range are replaced by one summary message.
    Summary {
        text: String,
        model: String,
        usage: Box<Usage>,
    },
}

/// Payload of a `compacted` event. Originals stay in the log; only the
/// projection changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactedPayload {
    /// Inclusive.
    pub from_seq: u64,
    /// Inclusive; always a turn boundary.
    pub to_seq: u64,
    pub strategy: CompactionStrategy,
}

/// Payload of a `pinned` event; the event's author is who pinned it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedPayload {
    pub text: String,
}

/// Payload of a `context_evicted` event (issue #30): within the turn the
/// event was appended in, tool results at or before `through_seq` and the
/// arguments of their successful `edit_file` / `write_file` calls are
/// stubbed in projection, except failed results and the last result of
/// each distinct tool. The originals stay in the log.
///
/// `ratio` (issue #52) is the calibration the sweep priced this move
/// with: the runtime's smoothed reported/estimate ratio, so a replay
/// reads the factor the decisions were taken under instead of
/// back-computing it from the log. Absent on logs written before it, and
/// on probes (`Runtime::sweep_decisions` decides the same way, but its
/// synthetic events are never stored).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextEvictedPayload {
    /// The last stubbed event's seq, inclusive.
    pub through_seq: u64,
    /// The sweep's calibration, when one was applied (issue #52).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ratio: Option<f64>,
}

/// Payload of a `context_saturated` event (issue #35): the eviction sweep
/// found the turn's material at the deepest legal boundary still over the
/// ceiling, so no move it can make fits the turn, and the thread is the
/// one to hand over. Machine-readable for whoever does: what the floor
/// holds, and what we are willing to pay for per call.
///
/// The event is never projected into model context, so this payload moves
/// no context whatever it says — the boundary it names is for the reader,
/// and may sit above the last one a `context_evicted` recorded.
///
/// Appended at most once per turn: the open turn's own scan keeps the
/// fact sticky from its first `context_saturated` until its `turn_ended`,
/// and a `context_evicted` does not re-arm it — the floor only grows
/// within a turn, so a spell and a turn coincide.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextSaturatedPayload {
    /// The floor's own boundary: the last call the sweep would stub,
    /// inclusive. Zero when the turn has no call it may stub yet.
    pub through_seq: u64,
    /// What the projection holds at that boundary in reported tokens
    /// (issue #52): the estimate the rule priced with, calibrated
    /// against the provider's own counts when the caller had a ratio.
    pub tokens_at_floor: u64,
    /// The ceiling it does not fit under.
    pub ceiling: u64,
    /// The sweep's calibration (issue #52); see
    /// [`ContextEvictedPayload::ratio`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ratio: Option<f64>,
}

/// Payload of a `provider_retried` event (issue #31): which retry is
/// starting, out of how many, why the call is quiet, and how long the
/// backoff waits before the next attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRetriedPayload {
    /// 1-based: the retry that is starting.
    pub attempt: u32,
    /// The most retries this call could make (issue #90): a ceiling
    /// derived from the retry window, so a turn line reading `retrying
    /// k/N` has N as the most there could be, not a promise.
    pub retries: u32,
    /// `not answering`, `http 503`, `overloaded`, prefixed with the
    /// profile label by the runtime.
    pub reason: String,
    /// The backoff before the next attempt.
    pub wait_ms: u64,
}

/// Payload of an `interrupted` event: appended on resume after a crash
/// (phase 2), or when a participant interrupts a running turn (phase 5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterruptedPayload {
    /// `process exited mid-turn`, or `interrupt` for a participant's.
    pub reason: String,
    /// Last event that was fully written before the crash.
    pub after_seq: u64,
    /// Tool call ids that received synthetic error results.
    #[serde(default)]
    pub unanswered_calls: Vec<String>,
    /// Who interrupted; `None` for a crash. Absent on lines from before
    /// phase 5.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<Author>,
}

/// Who invoked a skill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Invoker {
    /// A slash command in the client.
    User,
    /// The `load_skill` tool.
    Model,
}

/// Payload of a `skill_loaded` event: the body of a skill entered the
/// thread. `hash` and `source` are the lock entry's, so a regression can be
/// traced to a skill version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillLoadedPayload {
    pub name: String,
    pub hash: String,
    pub source: String,
    pub body: String,
    pub invoked_by: Invoker,
}

/// Payload of a `permission_requested` event: a tool call that policy
/// says a human must answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRequestedPayload {
    pub call: ToolCall,
    pub class: RiskClass,
    pub reason: String,
}

/// How long a human's answer holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionScope {
    /// This call only.
    Once,
    /// Every identical call until the process exits; never persisted.
    Session,
}

/// Payload of a `permission_decided` event; the event's author is who
/// answered and `parent_event` is the `permission_requested` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionDecidedPayload {
    pub call_id: String,
    pub allow: bool,
    pub scope: DecisionScope,
    /// Why, when it was not a person's choice: `interrupted` for a deny
    /// written because the turn was interrupted while waiting. Absent on
    /// lines from before phase 5.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// One line memory extraction wrote, with where it was stated so the
/// "only what a participant said" rule is auditable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryLine {
    /// File under the project's memory folder, e.g. `decisions.md`.
    pub file: String,
    pub text: String,
    pub stated_by: Author,
    /// The `user_message` (or a participant's `assistant_message`) seq
    /// the line came from.
    pub at_seq: u64,
}

/// Payload of a `memory_extracted` event, appended after a turn once the
/// project's memory files were updated. `through_seq` is the cursor the
/// next extraction starts after.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryExtractedPayload {
    /// Events considered, inclusive.
    pub through_seq: u64,
    /// What landed in the files; empty when the model found nothing new.
    #[serde(default)]
    pub written: Vec<MemoryLine>,
    /// The model that ran the extraction: the utility profile's when
    /// one is configured, else the thread's `model_label` (issue #18).
    pub model: String,
    pub usage: Usage,
}

/// Payload of a `memory_remembered` event (issue #14): a person ran
/// `/remember` and the runtime appended a line directly, no model call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRememberedPayload {
    /// The file under `.aigentic/memory/` the line went to.
    pub file: String,
    pub text: String,
    /// `false` when the line was already present and nothing was
    /// written; the event still records the ask.
    pub written: bool,
}

// ---- The build runner's payloads (layer 2, issue #53) ------------------
//
// The runner's state is a projection of the lead thread's log, so every
// fact it replays from has a shape here. Closed sets are enums with the
// existing `snake_case` rule; a new variant is the only way to add one.

/// How a step end left its work (PLAN-layer2 §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Done,
    Partial,
    Failed,
}

/// The `status` a child's `step_reported` may carry: a step that was cut
/// short reports `partial` and hands off, it does not report `failed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportStatus {
    Done,
    Partial,
}

/// One exact check's result (§6.1): `pass` and `flag` let the push
/// through, `fail` blocks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckResult {
    Pass,
    Flag,
    Fail,
}

/// The judge's verdict (§6.5). What to do about `changes_needed` is
/// [`FixSize`], a separate field: `fix is the judge's field`, and a
/// verdict is not a route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Approve,
    ChangesNeeded,
}

/// How much a `changes_needed` verdict needs: nothing, a small fix, or
/// the full route. `None` serialises as `none`, the word the judge
/// writes, not as `no_impact`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FixSize {
    #[serde(rename = "none")]
    None,
    Trivial,
    Full,
}

/// What a closed issue did to the release (ADR 0001). `NoImpact`
/// serialises as `none`, the word the judge writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseImpact {
    #[serde(rename = "none")]
    NoImpact,
    Patch,
    Minor,
    Breaking,
}

/// Whether a planned test landed, as the implementer's ledger claims it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerStatus {
    Landed,
    NotLanded,
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    /// The issue was closed.
    Closed,
    /// The run stopped at a recoverable point and left a handoff.
    Stopped,
    /// The run asked a human and could not go on.
    Escalated,
}

/// A human's answer at a checkpoint (§5). `stop` is `exec`'s default
/// when nobody is there; `go` never is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointAnswer {
    Go,
    Amend,
    Stop,
}

/// How a human marked a reveal-stage finding (§5, §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevealMark {
    Useful,
    Noise,
    Missed,
}

/// Whose budget a warning is about (§8): the step's or the issue's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetScope {
    Step,
    Issue,
}

/// A route a step may take (§6.3). One variant today; a new route is a
/// new variant, never a free-form string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Route {
    /// The step cannot go on; a human decides.
    Escalate { reason: String },
}

/// One commit the runner pushed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitRef {
    pub sha: String,
    pub subject: String,
}

/// One exact check's outcome (§6.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckOutcome {
    pub id: String,
    pub result: CheckResult,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Payload of a `run_started` event (issue #53): the issue, the workflow
/// the run began with (name, version and content hash, so a replay runs
/// against that workflow), and the workflow's provisional full budget.
/// The brief sizes the issue later, so there is no size here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunStartedPayload {
    pub issue: u64,
    pub workflow: String,
    /// `workflow.toml`'s `version`, an integer.
    pub version: u32,
    pub content_hash: String,
    pub budget_usd: f64,
}

/// Payload of a `step_started` event: which step, who fills it, in which
/// child thread, and at which attempt. Written before the child is
/// awaited; every re-entry is a new event with `attempt + 1`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepStartedPayload {
    pub step: String,
    pub role: String,
    pub profile: String,
    pub child_thread: Ulid,
    /// 1-based: the first entry is attempt 1.
    pub attempt: u32,
    pub budget_usd: f64,
    /// The repository's HEAD when a writing step started, on the first
    /// attempt only: the base every check and the push read the step's
    /// commits from. `None` on later attempts, and on a step that does
    /// not write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_at_start: Option<String>,
    /// The remote's head for the step's branch when the step started, on
    /// the first attempt only. `None` when the remote had no such branch,
    /// and on a step that does not write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_at_start: Option<String>,
}

/// Payload of a `step_finished` event: how the child's turn ended, what
/// it cost, and the child's `step_reported` event — `None` when the step
/// failed before it could report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepFinishedPayload {
    pub step: String,
    pub status: StepStatus,
    pub end_reason: String,
    pub cost_usd: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_event: Option<Ulid>,
}

/// Payload of a `checks_run` event: the step's exact checks and their
/// results. A `fail` blocks the push.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChecksRunPayload {
    pub step: String,
    pub checks: Vec<CheckOutcome>,
}

/// Payload of a `route_taken` event: the branch point, what the workflow
/// proposed, the preconditions with their results, what was taken, why a
/// fallback was needed, and the issue budget when the route set it (the
/// brief's route does).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteTakenPayload {
    pub branch: String,
    pub proposed: String,
    pub taken: String,
    #[serde(default)]
    pub preconditions: Vec<CheckOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_usd: Option<f64>,
}

/// Payload of a `checkpoint_asked` event: the gate, what the human was
/// shown, and the options offered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointAskedPayload {
    pub gate: String,
    pub shown: Vec<String>,
    pub options: Vec<String>,
}

/// One finding's mark at the reveal stage. `Missed` marks a purpose
/// point of the human's own amendment that purpose-check did not raise
/// (§5, §7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindingMark {
    pub finding: String,
    pub mark: RevealMark,
}

/// Payload of a `checkpoint_answered` event; the event's author is who
/// answered. The gate it answers is the open `checkpoint_asked`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckpointAnsweredPayload {
    pub answer: CheckpointAnswer,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amendment: Option<String>,
    #[serde(default)]
    pub marks: Vec<FindingMark>,
}

/// Payload of a `budget_warned` event: which budget, what was spent and
/// the limit. Both numbers are kept because §8 warns twice at each scope
/// (80% and 100%), so a restart needs the level, not just the scope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetWarnedPayload {
    pub scope: BudgetScope,
    pub spent_usd: f64,
    pub limit_usd: f64,
}

/// Payload of a `pushed` event: the commits, the remote ref before and
/// after, and the installed binary's commit. Appended after the push and
/// install, which are idempotent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PushedPayload {
    pub commits: Vec<CommitRef>,
    pub ref_before: String,
    pub ref_after: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed: Option<String>,
}

/// Payload of a `run_finished` event: how the run ended, what it cost,
/// and the release impact the closing comment states.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunFinishedPayload {
    pub outcome: RunOutcome,
    pub cost_usd: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_impact: Option<ReleaseImpact>,
}

/// One planned test: its id, what it checks, and how its expected value
/// is derived (§4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedTest {
    pub id: String,
    pub what: String,
    pub derivation: String,
}

/// One ledger row: a planned test and whether it landed, with the reason
/// when it did not (§4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub id: String,
    /// The test's name in the code.
    pub name: String,
    pub status: LedgerStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// A plan-check or purpose-check finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub id: String,
    pub text: String,
}

/// What a step hands over when it stops short (`status: partial`, §8):
/// what is done, what comes next, and the files left dirty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handoff {
    pub done: String,
    pub next: String,
    #[serde(default)]
    pub dirty: Vec<String>,
}

/// Payload of a `step_reported` event: the child's `finish_step` report,
/// §4's field table. Every field is optional because each step fills a
/// subset — a planner sends `slots` and `planned_tests`, a judge sends
/// `verdict` and `release_impact` — and none of them is written when
/// absent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct StepReport {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ReportStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Rendered template slots; `ui` is a bool, so the map holds JSON
    /// values and not strings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slots: Option<BTreeMap<String, serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planned_tests: Option<Vec<PlannedTest>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commits: Option<Vec<CommitRef>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ledger: Option<Vec<LedgerEntry>>,
    /// Phrases the report attributes to the plan, checked verbatim
    /// against it (§6.1 E8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quotes: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Verdict>,
    /// The judge's route for a `changes_needed` verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<FixSize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_impact: Option<ReleaseImpact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub findings: Option<Vec<Finding>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<Route>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<Handoff>,
}

/// What kind of decision a `decision_proposed` event is about (issue
/// #74). Every kind ADR 0002 names now is a variant from the start, so a
/// later producer adds no variant an older binary can't read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    /// Which project the thread is in.
    Project,
    /// What the build runner should do next with a job.
    Job,
    /// Which issue a ticket is, or how it is filed.
    Ticket,
    /// What is worth remembering.
    Knowledge,
    /// Which profile or model a step runs on.
    Route,
    /// Which files or contexts are in play.
    WorkingSet,
}

/// How much the harness is trusted to act on a decision kind (issue #74):
/// `ask` proposes and waits, `tell` proposes and states it, `silent` acts
/// without a proposal. Only `Ask` is produced in phase 6.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionStage {
    #[default]
    Ask,
    Tell,
    Silent,
}

/// Payload of a `decision_proposed` event (issue #74): the harness
/// proposes a decision of some kind. Written before the turn parks, so a
/// replay or resume finds the open proposal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionProposedPayload {
    pub kind: DecisionKind,
    /// What a person reads (`switch to getscale/site`).
    pub proposal: String,
    /// The machine-readable object (a project name, a job id) when there
    /// is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub reason: String,
    /// The tool call that made the proposal, when one did
    /// (`suggest_project`, `start_job`), so the runtime's pending entry
    /// and the log agree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(default)]
    pub stage: DecisionStage,
}

/// The `call_id` prefix of a proposal raised while no turn runs (issue
/// #85): #92's start-up proposal, which `suggest_project` never writes.
/// It is a value, not an event kind — an idle proposal is an ordinary
/// `decision_proposed` — so a log from before this reads as it did.
pub const STARTUP_PREFIX: &str = "startup-";

/// Whether a `call_id` names a proposal raised while no turn runs.
pub fn is_startup_call(call_id: Option<&str>) -> bool {
    call_id.is_some_and(|c| c.starts_with(STARTUP_PREFIX))
}

/// A `decision_answered` event's answer (issue #74). The first three are
/// **a person's** answers; `Withdrawn` closes a proposal nobody answered
/// — an interrupted turn, a restarted daemon, or `exec` declining
/// proposals non-interactively — and is written by `Author::System`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionAnswer {
    Yes,
    No,
    Corrected,
    Withdrawn,
}

/// Payload of a `decision_answered` event (issue #74); the event's author
/// is who answered and its `parent_event` is the proposal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionAnsweredPayload {
    pub answer: DecisionAnswer,
    /// Where the person pointed instead (`no, it's customer X`), present
    /// only with `Corrected`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correction: Option<String>,
    /// The answering path's own words (`turn interrupted`, `exec
    /// declines proposals`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The wire-name rule: `CamelCase` variant names become `snake_case`,
    /// so the expected name comes from the variant, not a hand-written
    /// pair (the runner kinds test's rule).
    fn snake_case(name: &str) -> String {
        let mut out = String::new();
        for (i, c) in name.char_indices() {
            if c.is_uppercase() {
                if i != 0 {
                    out.push('_');
                }
                out.extend(c.to_lowercase());
            } else {
                out.push(c);
            }
        }
        out
    }

    /// T2 (issue #74): both decision payloads round-trip, every
    /// `DecisionKind`, `DecisionStage` and `DecisionAnswer` variant
    /// serialises snake_case, a proposal written without `stage` reads as
    /// `ask`, and an absent optional key is absent on the wire, not
    /// `null`.
    #[test]
    fn decision_payloads_round_trip_in_snake_case() {
        for kind in [
            DecisionKind::Project,
            DecisionKind::Job,
            DecisionKind::Ticket,
            DecisionKind::Knowledge,
            DecisionKind::Route,
            DecisionKind::WorkingSet,
        ] {
            let wire = serde_json::to_value(kind).unwrap();
            assert_eq!(wire, json!(snake_case(&format!("{kind:?}"))));
        }
        for stage in [
            DecisionStage::Ask,
            DecisionStage::Tell,
            DecisionStage::Silent,
        ] {
            let wire = serde_json::to_value(stage).unwrap();
            assert_eq!(wire, json!(snake_case(&format!("{stage:?}"))));
        }
        for answer in [
            DecisionAnswer::Yes,
            DecisionAnswer::No,
            DecisionAnswer::Corrected,
            DecisionAnswer::Withdrawn,
        ] {
            let wire = serde_json::to_value(answer).unwrap();
            assert_eq!(wire, json!(snake_case(&format!("{answer:?}"))));
        }

        // A full proposal round-trips, with `target` and `call_id` set.
        let proposed = DecisionProposedPayload {
            kind: DecisionKind::Project,
            proposal: "switch to getscale/site".into(),
            target: Some("getscale/site".into()),
            reason: "the brief names it".into(),
            call_id: Some("c1".into()),
            stage: DecisionStage::Ask,
        };
        let value = serde_json::to_value(&proposed).unwrap();
        assert_eq!(value["kind"], "project");
        assert_eq!(value["stage"], "ask");
        assert_eq!(
            serde_json::from_value::<DecisionProposedPayload>(value).unwrap(),
            proposed
        );

        // A proposal written without `stage` reads as `ask`, and absent
        // `target`/`call_id` are absent on the wire, not `null`.
        let bare = serde_json::from_value::<DecisionProposedPayload>(json!({
            "kind": "job",
            "proposal": "start the build for #77",
            "reason": "the issue is ready"
        }))
        .unwrap();
        assert_eq!(bare.stage, DecisionStage::Ask);
        assert_eq!(bare.target, None);
        assert_eq!(bare.call_id, None);
        let back = serde_json::to_value(&bare).unwrap();
        for absent in ["target", "call_id"] {
            assert!(back.get(absent).is_none(), "{absent} is absent: {back}");
        }
        assert_eq!(back["stage"], "ask");

        // An answer round-trips; a `corrected` carries its correction.
        let answered = DecisionAnsweredPayload {
            answer: DecisionAnswer::Corrected,
            correction: Some("no, it's customer X".into()),
            note: None,
        };
        let value = serde_json::to_value(&answered).unwrap();
        assert_eq!(
            value,
            json!({"answer": "corrected", "correction": "no, it's customer X"})
        );
        assert_eq!(
            serde_json::from_value::<DecisionAnsweredPayload>(value).unwrap(),
            answered
        );

        // A withdrawal is written by the system with its own note; the
        // absent `correction` is absent on the wire, not `null`.
        let withdrawn = DecisionAnsweredPayload {
            answer: DecisionAnswer::Withdrawn,
            correction: None,
            note: Some("turn interrupted".into()),
        };
        let value = serde_json::to_value(&withdrawn).unwrap();
        assert_eq!(
            value,
            json!({"answer": "withdrawn", "note": "turn interrupted"})
        );
        assert!(value.get("correction").is_none());
    }

    #[test]
    fn phase5_fields_default_and_stay_off_old_lines() {
        // A phase 0 line has no `mid_turn`; it reads back false and a
        // false value is not written, so old shapes are byte-stable.
        let p: UserMessagePayload =
            serde_json::from_value(json!({"blocks": [{"type": "text", "text": "hi"}]})).unwrap();
        assert!(!p.mid_turn);
        assert_eq!(
            serde_json::to_value(&p).unwrap(),
            json!({"blocks": [{"type": "text", "text": "hi"}]})
        );
        let queued = UserMessagePayload {
            mid_turn: true,
            ..UserMessagePayload::new(vec![ContentBlock::Text("later".into())])
        };
        let value = serde_json::to_value(&queued).unwrap();
        assert_eq!(value["mid_turn"], true);
        assert_eq!(
            serde_json::from_value::<UserMessagePayload>(value).unwrap(),
            queued
        );

        // A steered line (issue #33) carries the flag; one without it,
        // including every pre-steering log line, reads back false.
        let steered = UserMessagePayload {
            mid_turn: true,
            steer: true,
            ..UserMessagePayload::new(vec![ContentBlock::Text("now".into())])
        };
        let value = serde_json::to_value(&steered).unwrap();
        assert_eq!(value["steer"], true);
        assert_eq!(
            serde_json::from_value::<UserMessagePayload>(value).unwrap(),
            steered
        );
        let p: UserMessagePayload = serde_json::from_value(
            json!({"blocks": [{"type": "text", "text": "hi"}], "mid_turn": true}),
        )
        .unwrap();
        assert!(p.mid_turn);
        assert!(!p.steer);

        let p: InterruptedPayload = serde_json::from_value(
            json!({"reason": "process exited mid-turn", "after_seq": 5, "unanswered_calls": []}),
        )
        .unwrap();
        assert_eq!(p.by, None);
        assert!(serde_json::to_value(&p).unwrap().get("by").is_none());
        let by_steve = InterruptedPayload {
            reason: "interrupt".into(),
            after_seq: 9,
            unanswered_calls: vec![],
            by: Some(Author::User(aigentic_core::UserId("steve".into()))),
        };
        let value = serde_json::to_value(&by_steve).unwrap();
        assert_eq!(value["by"], json!({"kind": "user", "id": "steve"}));
        assert_eq!(
            serde_json::from_value::<InterruptedPayload>(value).unwrap(),
            by_steve
        );

        let started = ThreadStartedPayload {
            project: Some("vendela".into()),
            root: PathBuf::from("/srv/vendela"),
            created_by: Author::User(aigentic_core::UserId("steve".into())),
            parent_thread: None,
            step: None,
            front: false,
        };
        let value = serde_json::to_value(&started).unwrap();
        assert_eq!(
            value,
            json!({"project": "vendela", "root": "/srv/vendela", "created_by": {"kind": "user", "id": "steve"}})
        );
        assert_eq!(
            serde_json::from_value::<ThreadStartedPayload>(value).unwrap(),
            started
        );
        let none: ThreadStartedPayload = serde_json::from_value(
            json!({"project": null, "root": "/tmp/x", "created_by": {"kind": "system"}}),
        )
        .unwrap();
        assert_eq!(none.project, None);
    }

    #[test]
    fn memory_extracted_round_trips_with_an_empty_written_default() {
        let p = MemoryExtractedPayload {
            through_seq: 12,
            written: vec![MemoryLine {
                file: "decisions.md".into(),
                text: "Use Swedish.".into(),
                stated_by: Author::User(aigentic_core::UserId("steve".into())),
                at_seq: 3,
            }],
            model: "m".into(),
            usage: Usage::reported(aigentic_core::Usage::default()),
        };
        let value = serde_json::to_value(&p).unwrap();
        assert_eq!(
            value["written"][0]["stated_by"],
            json!({"kind": "user", "id": "steve"})
        );
        assert_eq!(
            serde_json::from_value::<MemoryExtractedPayload>(value).unwrap(),
            p
        );
        let bare: MemoryExtractedPayload = serde_json::from_value(json!({
            "through_seq": 1, "model": "m",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }))
        .unwrap();
        assert!(bare.written.is_empty());
    }

    #[test]
    fn thread_renamed_keeps_its_old_shape_without_a_call_behind_it() {
        // An old line, and a person's `/rename`: a title and nothing
        // else, written back byte for byte (issue #49).
        let bare: ThreadRenamedPayload = serde_json::from_value(json!({"title": "x"})).unwrap();
        assert_eq!(bare.model, None);
        assert_eq!(bare.usage, None);
        assert_eq!(serde_json::to_value(&bare).unwrap(), json!({"title": "x"}));

        // A utility model's line carries the model and the call's usage,
        // and round-trips.
        let called = ThreadRenamedPayload {
            title: "x".into(),
            model: Some("utility-model".into()),
            usage: Some(Usage::reported(aigentic_core::Usage {
                input_tokens: 300,
                output_tokens: 20,
                ..Default::default()
            })),
        };
        let value = serde_json::to_value(&called).unwrap();
        assert_eq!(value["model"], "utility-model");
        assert_eq!(value["usage"]["input_tokens"], 300);
        assert_eq!(
            serde_json::from_value::<ThreadRenamedPayload>(value).unwrap(),
            called
        );
    }

    #[test]
    fn phase0_tool_result_lines_read_back_without_policy() {
        let line = json!({"id": "c1", "content": "ok", "is_error": false});
        let p: ToolResultPayload = serde_json::from_value(line).unwrap();
        assert_eq!(p.result.id, "c1");
        assert_eq!(p.policy, None);
    }

    #[test]
    fn context_saturated_round_trips_with_three_fields() {
        let p = ContextSaturatedPayload {
            through_seq: 41,
            tokens_at_floor: 137_000,
            ceiling: 128_000,
            ratio: None,
        };
        let value = serde_json::to_value(&p).unwrap();
        assert_eq!(
            value,
            json!({"through_seq": 41, "tokens_at_floor": 137_000, "ceiling": 128_000})
        );
        assert_eq!(
            serde_json::from_value::<ContextSaturatedPayload>(value).unwrap(),
            p
        );
    }

    #[test]
    fn context_evicted_round_trips_with_one_field() {
        let p = ContextEvictedPayload {
            through_seq: 41,
            ratio: None,
        };
        let value = serde_json::to_value(&p).unwrap();
        assert_eq!(value, json!({"through_seq": 41}));
        assert_eq!(
            serde_json::from_value::<ContextEvictedPayload>(value).unwrap(),
            p
        );
    }

    /// T5, issue #52: the sweep's learned calibration is logged, so a
    /// replay reads the factor the decisions were taken under rather
    /// than back-computing it. A payload written without it (every log
    /// before #52) still reads back, with the ratio absent.
    #[test]
    fn sweep_payloads_round_trip_with_and_without_the_calibration() {
        let evicted = ContextEvictedPayload {
            through_seq: 41,
            ratio: Some(1.39),
        };
        let value = serde_json::to_value(&evicted).unwrap();
        assert_eq!(value, json!({"through_seq": 41, "ratio": 1.39}));
        assert_eq!(
            serde_json::from_value::<ContextEvictedPayload>(value).unwrap(),
            evicted
        );

        let saturated = ContextSaturatedPayload {
            through_seq: 41,
            tokens_at_floor: 137_000,
            ceiling: 128_000,
            ratio: Some(1.39),
        };
        let value = serde_json::to_value(&saturated).unwrap();
        assert_eq!(
            value,
            json!({
                "through_seq": 41,
                "tokens_at_floor": 137_000,
                "ceiling": 128_000,
                "ratio": 1.39,
            })
        );
        assert_eq!(
            serde_json::from_value::<ContextSaturatedPayload>(value).unwrap(),
            saturated
        );

        // Legacy JSON: the field is absent, and absence is `None`.
        let legacy = json!({"through_seq": 17});
        let p: ContextEvictedPayload = serde_json::from_value(legacy).unwrap();
        assert_eq!(p.through_seq, 17);
        assert_eq!(p.ratio, None);
        let legacy = json!({"through_seq": 17, "tokens_at_floor": 5, "ceiling": 4});
        let p: ContextSaturatedPayload = serde_json::from_value(legacy).unwrap();
        assert_eq!(p.tokens_at_floor, 5);
        assert_eq!(p.ratio, None);
    }

    #[test]
    fn tool_result_wire_shape_is_flat_with_optional_policy() {
        let result = ToolResult {
            id: "c1".into(),
            content: "ok".into(),
            is_error: false,
        };
        let bare = ToolResultPayload {
            result: result.clone(),
            policy: None,
        };
        assert_eq!(
            serde_json::to_value(&bare).unwrap(),
            json!({"id": "c1", "content": "ok", "is_error": false})
        );

        let by_rule =
            ToolResultPayload::new(result.clone(), PolicyRecord::rule("class read", "allow"));
        let value = serde_json::to_value(&by_rule).unwrap();
        assert_eq!(
            value["policy"],
            json!({"kind": "rule", "rule": "class read", "decision": "allow"})
        );
        assert_eq!(
            serde_json::from_value::<ToolResultPayload>(value).unwrap(),
            by_rule
        );

        let event = Ulid::from_parts(1_700_000_000_000, 7);
        let by_human = ToolResultPayload::new(result, PolicyRecord::Human { event, allow: true });
        let value = serde_json::to_value(&by_human).unwrap();
        assert_eq!(
            value["policy"],
            json!({"kind": "human", "event": event.to_string(), "allow": true})
        );
        assert_eq!(
            serde_json::from_value::<ToolResultPayload>(value).unwrap(),
            by_human
        );
    }

    #[test]
    fn phase3_payloads_round_trip_in_snake_case() {
        let loaded = SkillLoadedPayload {
            name: "tdd".into(),
            hash: "abc".into(),
            source: "github.com/mattpocock/skills@c55ee46".into(),
            body: "# TDD".into(),
            invoked_by: Invoker::Model,
        };
        let value = serde_json::to_value(&loaded).unwrap();
        assert_eq!(value["invoked_by"], "model");
        assert_eq!(
            serde_json::from_value::<SkillLoadedPayload>(value).unwrap(),
            loaded
        );

        let requested = PermissionRequestedPayload {
            call: ToolCall {
                id: "c2".into(),
                name: "bash".into(),
                args: json!({"command": "rm -rf build"}),
            },
            class: RiskClass::Exec,
            reason: "class exec: ask".into(),
        };
        let value = serde_json::to_value(&requested).unwrap();
        assert_eq!(value["class"], "exec");
        assert_eq!(value["call"]["name"], "bash");
        assert_eq!(
            serde_json::from_value::<PermissionRequestedPayload>(value).unwrap(),
            requested
        );

        let decided = PermissionDecidedPayload {
            call_id: "c2".into(),
            allow: false,
            scope: DecisionScope::Session,
            reason: None,
        };
        let value = serde_json::to_value(&decided).unwrap();
        assert_eq!(
            value,
            json!({"call_id": "c2", "allow": false, "scope": "session"})
        );
        assert_eq!(
            serde_json::from_value::<PermissionDecidedPayload>(value).unwrap(),
            decided
        );
    }

    /// Issue #31: a `usage` line written before the five stamping fields
    /// existed still parses — all five are `None` — and none of them is
    /// written back, so an old log re-serialises as it always did (the
    /// only other absent lines are the pre-existing optional ones).
    #[test]
    fn a_pre_stamp_usage_line_gains_no_new_fields() {
        let line = json!({
            "input_tokens": 100,
            "output_tokens": 20,
            "cache_read_tokens": 5,
            "cache_write_tokens": 0,
            "estimated": false
        });
        let usage: Usage = serde_json::from_value(line.clone()).unwrap();
        assert_eq!(usage.profile, None);
        assert_eq!(usage.model, None);
        assert_eq!(usage.effort, None);
        assert_eq!(usage.latency_ms, None);
        assert_eq!(usage.ttft_ms, None);
        assert_eq!(usage.cost_usd, None);
        let back = serde_json::to_value(&usage).unwrap();
        for absent in [
            "profile",
            "model",
            "effort",
            "latency_ms",
            "ttft_ms",
            "cost_usd",
        ] {
            assert!(
                back.get(absent).is_none(),
                "{absent} is absent from a pre-stamp line: {back}"
            );
        }
        for (key, value) in line.as_object().unwrap() {
            assert_eq!(back.get(key), Some(value), "{key} is unchanged");
        }
    }

    /// And a stamped one round-trips field for field.
    #[test]
    fn a_stamped_usage_line_round_trips() {
        let usage = Usage {
            input_tokens: 1,
            output_tokens: 2,
            cache_read_tokens: 3,
            cache_write_tokens: 4,
            reasoning_tokens: Some(5),
            estimated: false,
            profile: Some("tensorx".into()),
            model: Some("deepseek-v4".into()),
            effort: Some("50".into()),
            latency_ms: Some(1500),
            ttft_ms: Some(250),
            cost_usd: Some(0.25),
        };
        let value = serde_json::to_value(&usage).unwrap();
        assert_eq!(value["profile"], "tensorx");
        assert_eq!(value["effort"], "50");
        assert_eq!(value["cost_usd"], 0.25);
        assert_eq!(serde_json::from_value::<Usage>(value).unwrap(), usage);
    }

    /// The retry payload's wire shape: snake_case keys, no surprises.
    #[test]
    fn a_provider_retried_payload_round_trips() {
        let payload = ProviderRetriedPayload {
            attempt: 2,
            retries: 3,
            reason: "tensorx · not answering".into(),
            wait_ms: 3000,
        };
        let value = serde_json::to_value(&payload).unwrap();
        assert_eq!(
            value,
            json!({
                "attempt": 2,
                "retries": 3,
                "reason": "tensorx · not answering",
                "wait_ms": 3000
            })
        );
        assert_eq!(
            serde_json::from_value::<ProviderRetriedPayload>(value).unwrap(),
            payload
        );
    }

    /// Issue #47: a `turn_ended` line written before the sleep fields
    /// existed still parses — every new field is `None` — and none of
    /// them is written back, so an old log re-serialises as it always
    /// did.
    #[test]
    fn a_pre_sleep_turn_ended_line_gains_no_new_fields() {
        let line = json!({"reason": "done", "touched": ["a.rs"]});
        let p: TurnEndedPayload = serde_json::from_value(line.clone()).unwrap();
        assert_eq!(p.wall_secs, None);
        assert_eq!(p.slept_secs, None);
        assert_eq!(p.slept_awaiting_secs, None);
        assert_eq!(p.keep_awake, None);
        let back = serde_json::to_value(&p).unwrap();
        assert_eq!(back, line);
        for absent in [
            "wall_secs",
            "slept_secs",
            "slept_awaiting_secs",
            "keep_awake",
            "error",
        ] {
            assert!(
                back.get(absent).is_none(),
                "{absent} is absent from a pre-sleep line: {back}"
            );
        }

        // A measured turn round-trips every field.
        let measured = TurnEndedPayload {
            reason: "done".into(),
            touched: vec![],
            wall_secs: Some(980),
            slept_secs: Some(750),
            slept_awaiting_secs: Some(120),
            keep_awake: Some("on".into()),
            error: None,
        };
        let value = serde_json::to_value(&measured).unwrap();
        assert_eq!(value["wall_secs"], 980);
        assert_eq!(value["slept_secs"], 750);
        assert_eq!(value["slept_awaiting_secs"], 120);
        assert_eq!(value["keep_awake"], "on");
        assert_eq!(
            serde_json::from_value::<TurnEndedPayload>(value).unwrap(),
            measured
        );
    }

    /// T6 (issue #22): a `turn_ended` line written before the `error`
    /// field existed parses with `error: None` and writes nothing back.
    #[test]
    fn turn_ended_payload_without_error_field_deserializes() {
        let line = json!({"reason": "provider_error: http 503: boom"});
        let p: TurnEndedPayload = serde_json::from_value(line.clone()).unwrap();
        assert_eq!(p.error, None);
        assert_eq!(serde_json::to_value(&p).unwrap(), line);
    }

    /// The amendment to issue #22: an `error` this binary cannot read
    /// (an unknown variant) degrades to `None` instead of failing the
    /// whole `turn_ended` line, so the stored raw `reason` still replays.
    #[test]
    fn an_unreadable_error_degrades_to_none() {
        let line = json!({
            "reason": "provider_error: http 503: boom",
            "error": {"a_variant_from_a_later_version": {"detail": "?"}}
        });
        let p: TurnEndedPayload = serde_json::from_value(line).unwrap();
        assert_eq!(p.error, None);
        assert_eq!(p.reason, "provider_error: http 503: boom");
    }

    /// And a readable error round-trips as its machine shape.
    #[test]
    fn a_readable_error_round_trips() {
        let p = TurnEndedPayload {
            error: Some(ProviderError::Http {
                status: 503,
                body: "boom".into(),
            }),
            ..TurnEndedPayload::new("provider_error: http 503: boom")
        };
        let value = serde_json::to_value(&p).unwrap();
        assert_eq!(value["error"]["Http"]["status"], 503);
        assert_eq!(
            serde_json::from_value::<TurnEndedPayload>(value).unwrap(),
            p
        );
    }

    // ---- The build runner's payloads (layer 2, issue #53) -------------
    //
    // One round trip per new payload, with the expected JSON written from
    // the serde rule (`snake_case` keys, absent options omitted) rather
    // than from a second renderer.

    fn a_child_thread() -> Ulid {
        Ulid::from_parts(1_700_000_000_000, 7)
    }

    #[test]
    fn run_started_payload_round_trips() {
        let p = RunStartedPayload {
            issue: 53,
            workflow: "build".into(),
            version: 1,
            content_hash: "abc123".into(),
            budget_usd: 10.0,
        };
        let value = serde_json::to_value(&p).unwrap();
        assert_eq!(
            value,
            json!({
                "issue": 53,
                "workflow": "build",
                "version": 1,
                "content_hash": "abc123",
                "budget_usd": 10.0
            })
        );
        assert_eq!(
            serde_json::from_value::<RunStartedPayload>(value).unwrap(),
            p
        );
    }

    #[test]
    fn step_started_payload_round_trips() {
        // A writing step's first attempt records where it started (#65):
        // the head its commits are read from and the remote ref its push
        // is judged against.
        let p = StepStartedPayload {
            step: "plan".into(),
            role: "planner".into(),
            profile: "kimi".into(),
            child_thread: a_child_thread(),
            attempt: 1,
            budget_usd: 3.0,
            head_at_start: Some("a".repeat(40)),
            remote_at_start: Some("b".repeat(40)),
        };
        let value = serde_json::to_value(&p).unwrap();
        assert_eq!(
            value,
            json!({
                "step": "plan",
                "role": "planner",
                "profile": "kimi",
                "child_thread": a_child_thread().to_string(),
                "attempt": 1,
                "budget_usd": 3.0,
                "head_at_start": "a".repeat(40),
                "remote_at_start": "b".repeat(40),
            })
        );
        assert_eq!(
            serde_json::from_value::<StepStartedPayload>(value).unwrap(),
            p
        );

        // A step that does not write records neither, and the fields are
        // left out of the line rather than written as nulls.
        let quiet = StepStartedPayload {
            head_at_start: None,
            remote_at_start: None,
            ..p.clone()
        };
        let value = serde_json::to_value(&quiet).unwrap();
        assert!(value.get("head_at_start").is_none(), "{value}");
        assert!(value.get("remote_at_start").is_none(), "{value}");

        // An event written before #65 has neither field: it still reads,
        // with both `None`, so an old log replays.
        let old = json!({
            "step": "plan",
            "role": "planner",
            "profile": "kimi",
            "child_thread": a_child_thread().to_string(),
            "attempt": 1,
            "budget_usd": 3.0
        });
        assert_eq!(
            serde_json::from_value::<StepStartedPayload>(old).unwrap(),
            quiet
        );
    }

    #[test]
    fn step_finished_payload_round_trips_with_and_without_a_report() {
        let reported = Ulid::from_parts(1_700_000_000_000, 8);
        let p = StepFinishedPayload {
            step: "plan".into(),
            status: StepStatus::Partial,
            end_reason: "max_tokens".into(),
            cost_usd: 0.42,
            reported_event: Some(reported),
        };
        let value = serde_json::to_value(&p).unwrap();
        assert_eq!(value["status"], "partial");
        assert_eq!(value["reported_event"], reported.to_string());
        assert_eq!(
            value,
            json!({
                "step": "plan",
                "status": "partial",
                "end_reason": "max_tokens",
                "cost_usd": 0.42,
                "reported_event": reported.to_string()
            })
        );
        assert_eq!(
            serde_json::from_value::<StepFinishedPayload>(value).unwrap(),
            p
        );

        // A step that failed before it could report writes no key at all.
        let failed = StepFinishedPayload {
            reported_event: None,
            ..p
        };
        let value = serde_json::to_value(&failed).unwrap();
        assert!(value.get("reported_event").is_none(), "{value}");
        assert_eq!(
            serde_json::from_value::<StepFinishedPayload>(value).unwrap(),
            failed
        );
    }

    #[test]
    fn checks_run_payload_round_trips() {
        let p = ChecksRunPayload {
            step: "implement".into(),
            checks: vec![
                CheckOutcome {
                    id: "E1".into(),
                    result: CheckResult::Pass,
                    detail: None,
                },
                CheckOutcome {
                    id: "E8".into(),
                    result: CheckResult::Fail,
                    detail: Some("quotes do not appear in the plan".into()),
                },
                CheckOutcome {
                    id: "E9".into(),
                    result: CheckResult::Flag,
                    detail: None,
                },
            ],
        };
        let value = serde_json::to_value(&p).unwrap();
        assert_eq!(
            value,
            json!({
                "step": "implement",
                "checks": [
                    {"id": "E1", "result": "pass"},
                    {"id": "E8", "result": "fail",
                     "detail": "quotes do not appear in the plan"},
                    {"id": "E9", "result": "flag"}
                ]
            })
        );
        assert_eq!(
            serde_json::from_value::<ChecksRunPayload>(value).unwrap(),
            p
        );
    }

    #[test]
    fn route_taken_payload_round_trips() {
        let p = RouteTakenPayload {
            branch: "size".into(),
            proposed: "full".into(),
            taken: "trivial".into(),
            preconditions: vec![CheckOutcome {
                id: "small_diff".into(),
                result: CheckResult::Pass,
                detail: None,
            }],
            fallback_reason: Some("one-line fix".into()),
            budget_usd: Some(2.5),
        };
        let value = serde_json::to_value(&p).unwrap();
        assert_eq!(
            value,
            json!({
                "branch": "size",
                "proposed": "full",
                "taken": "trivial",
                "preconditions": [{"id": "small_diff", "result": "pass"}],
                "fallback_reason": "one-line fix",
                "budget_usd": 2.5
            })
        );
        assert_eq!(
            serde_json::from_value::<RouteTakenPayload>(value).unwrap(),
            p
        );

        // A route that does not fix the budget leaves the key off; the
        // preconditions list is always written, empty when none ran.
        let bare = RouteTakenPayload {
            preconditions: vec![],
            fallback_reason: None,
            budget_usd: None,
            ..p
        };
        let value = serde_json::to_value(&bare).unwrap();
        assert_eq!(
            value,
            json!({
                "branch": "size",
                "proposed": "full",
                "taken": "trivial",
                "preconditions": []
            })
        );
    }

    #[test]
    fn checkpoint_asked_payload_round_trips() {
        let p = CheckpointAskedPayload {
            gate: "plan_gate".into(),
            shown: vec!["## Plan".into(), "## Plan amendment".into()],
            options: vec!["go".into(), "amend".into(), "stop".into()],
        };
        let value = serde_json::to_value(&p).unwrap();
        assert_eq!(
            value,
            json!({
                "gate": "plan_gate",
                "shown": ["## Plan", "## Plan amendment"],
                "options": ["go", "amend", "stop"]
            })
        );
        assert_eq!(
            serde_json::from_value::<CheckpointAskedPayload>(value).unwrap(),
            p
        );
    }

    #[test]
    fn checkpoint_answered_payload_round_trips() {
        let p = CheckpointAnsweredPayload {
            answer: CheckpointAnswer::Amend,
            amendment: Some("the plan said T3, code says 32,000".into()),
            marks: vec![FindingMark {
                finding: "F2".into(),
                mark: RevealMark::Missed,
            }],
        };
        let value = serde_json::to_value(&p).unwrap();
        assert_eq!(
            value,
            json!({
                "answer": "amend",
                "amendment": "the plan said T3, code says 32,000",
                "marks": [{"finding": "F2", "mark": "missed"}]
            })
        );
        assert_eq!(
            serde_json::from_value::<CheckpointAnsweredPayload>(value).unwrap(),
            p
        );

        // `stop` is exec's default when nobody is there, so it is a real
        // answer on the wire and not an absence.
        let stopped = CheckpointAnsweredPayload {
            answer: CheckpointAnswer::Stop,
            amendment: None,
            marks: vec![],
        };
        let value = serde_json::to_value(&stopped).unwrap();
        assert_eq!(value, json!({"answer": "stop", "marks": []}));
        assert_eq!(
            serde_json::from_value::<CheckpointAnsweredPayload>(value).unwrap(),
            stopped
        );
    }

    #[test]
    fn budget_warned_payload_round_trips() {
        let p = BudgetWarnedPayload {
            scope: BudgetScope::Issue,
            spent_usd: 8.0,
            limit_usd: 10.0,
        };
        let value = serde_json::to_value(&p).unwrap();
        assert_eq!(
            value,
            json!({"scope": "issue", "spent_usd": 8.0, "limit_usd": 10.0})
        );
        assert_eq!(
            serde_json::from_value::<BudgetWarnedPayload>(value).unwrap(),
            p
        );
    }

    #[test]
    fn pushed_payload_round_trips() {
        let p = PushedPayload {
            commits: vec![CommitRef {
                sha: "abc123".into(),
                subject: "log: project a lead thread into a RunState".into(),
            }],
            ref_before: "abc000".into(),
            ref_after: "abc123".into(),
            installed: Some("abc123".into()),
        };
        let value = serde_json::to_value(&p).unwrap();
        assert_eq!(
            value,
            json!({
                "commits": [{"sha": "abc123",
                             "subject": "log: project a lead thread into a RunState"}],
                "ref_before": "abc000",
                "ref_after": "abc123",
                "installed": "abc123"
            })
        );
        assert_eq!(serde_json::from_value::<PushedPayload>(value).unwrap(), p);

        // An install that did not happen yet is absent, not null.
        let uninstalled = PushedPayload {
            installed: None,
            ..p
        };
        let value = serde_json::to_value(&uninstalled).unwrap();
        assert!(value.get("installed").is_none(), "{value}");
    }

    #[test]
    fn run_finished_payload_round_trips() {
        let p = RunFinishedPayload {
            outcome: RunOutcome::Closed,
            cost_usd: 1.25,
            release_impact: Some(ReleaseImpact::NoImpact),
        };
        let value = serde_json::to_value(&p).unwrap();
        // `none` is the word the judge writes, not the variant's own name.
        assert_eq!(
            value,
            json!({"outcome": "closed", "cost_usd": 1.25, "release_impact": "none"})
        );
        assert_eq!(
            serde_json::from_value::<RunFinishedPayload>(value).unwrap(),
            p
        );
        assert_eq!(serde_json::to_value(ReleaseImpact::Patch).unwrap(), "patch");
        assert_eq!(serde_json::to_value(ReleaseImpact::Minor).unwrap(), "minor");
        assert_eq!(
            serde_json::to_value(ReleaseImpact::Breaking).unwrap(),
            "breaking"
        );

        let stopped = RunFinishedPayload {
            outcome: RunOutcome::Stopped,
            release_impact: None,
            ..p
        };
        let value = serde_json::to_value(&stopped).unwrap();
        assert_eq!(value, json!({"outcome": "stopped", "cost_usd": 1.25}));
        assert_eq!(
            serde_json::to_value(RunOutcome::Escalated).unwrap(),
            "escalated"
        );
    }

    /// Amendment item 4: `fix` is the judge's own field, its no-change
    /// variant is the word `none`, and a verdict is not a route.
    #[test]
    fn fix_size_and_verdict_have_their_own_wire_names() {
        assert_eq!(serde_json::to_value(FixSize::None).unwrap(), "none");
        assert_eq!(serde_json::to_value(FixSize::Trivial).unwrap(), "trivial");
        assert_eq!(serde_json::to_value(FixSize::Full).unwrap(), "full");
        assert_eq!(
            serde_json::to_value(Verdict::ChangesNeeded).unwrap(),
            "changes_needed"
        );
        assert_eq!(serde_json::to_value(Verdict::Approve).unwrap(), "approve");
    }

    /// A route is tagged by `kind`, like the other tagged unions; a new
    /// route is a new variant, never a free-form string.
    #[test]
    fn a_route_is_tagged_by_kind() {
        let route = Route::Escalate {
            reason: "the judge found the plan's premise false".into(),
        };
        let value = serde_json::to_value(&route).unwrap();
        assert_eq!(
            value,
            json!({"kind": "escalate", "reason": "the judge found the plan's premise false"})
        );
        assert_eq!(serde_json::from_value::<Route>(value).unwrap(), route);
    }

    #[test]
    fn a_full_step_report_round_trips() {
        let p = StepReport {
            status: Some(ReportStatus::Done),
            body: Some("## Implementation".into()),
            slots: Some(BTreeMap::from([
                ("ui".to_string(), json!(true)),
                ("issue".to_string(), json!(53)),
            ])),
            planned_tests: Some(vec![PlannedTest {
                id: "T1".into(),
                what: "the new kinds round-trip".into(),
                derivation: "the enum's snake_case rule".into(),
            }]),
            commits: Some(vec![CommitRef {
                sha: "abc123".into(),
                subject: "core, log: add the build runner's event kinds and payloads".into(),
            }]),
            ledger: Some(vec![
                LedgerEntry {
                    id: "T1".into(),
                    name: "the_runner_kinds_round_trip_under_the_snake_case_rule".into(),
                    status: LedgerStatus::Landed,
                    reason: None,
                },
                LedgerEntry {
                    id: "T2".into(),
                    name: "run_started_payload_round_trips".into(),
                    status: LedgerStatus::NotLanded,
                    reason: Some("the payload changed shape in an amendment".into()),
                },
            ]),
            quotes: Some(vec!["no hand-computed literals".into()]),
            verdict: Some(Verdict::ChangesNeeded),
            fix: Some(FixSize::Trivial),
            release_impact: Some(ReleaseImpact::Patch),
            findings: Some(vec![Finding {
                id: "F1".into(),
                text: "the ledger's T9 has no test".into(),
            }]),
            route: Some(Route::Escalate {
                reason: "an ordering the workflow cannot route".into(),
            }),
            handoff: Some(Handoff {
                done: "kinds and payloads".into(),
                next: "run_state.rs".into(),
                dirty: vec!["crates/log/src/payload.rs".into()],
            }),
        };
        let value = serde_json::to_value(&p).unwrap();
        assert!(value.get("reported_event").is_none(), "not a report field");
        // The `ui` slot is a bool (a later ticket's templates), and the
        // map keeps it one.
        assert_eq!(value["slots"]["ui"], json!(true));
        assert_eq!(value["status"], "done");
        assert_eq!(value["verdict"], "changes_needed");
        assert_eq!(value["fix"], "trivial");
        assert_eq!(value["release_impact"], "patch");
        assert_eq!(value["ledger"][0]["status"], "landed");
        assert_eq!(value["ledger"][1]["status"], "not_landed");
        assert!(value["ledger"][0].get("reason").is_none(), "{value}");
        assert_eq!(value["findings"][0]["id"], "F1");
        assert_eq!(
            value["route"],
            json!({"kind": "escalate",
            "reason": "an ordering the workflow cannot route"})
        );
        assert_eq!(
            value["handoff"]["dirty"],
            json!(["crates/log/src/payload.rs"])
        );
        assert_eq!(serde_json::from_value::<StepReport>(value).unwrap(), p);
    }

    /// A step fills a subset, so a report may carry one field and none of
    /// the others — and the absent ones stay off the line.
    #[test]
    fn a_step_report_may_carry_only_a_body() {
        let p = StepReport {
            body: Some("## Review".into()),
            ..StepReport::default()
        };
        let value = serde_json::to_value(&p).unwrap();
        assert_eq!(value, json!({"body": "## Review"}));
        assert_eq!(serde_json::from_value::<StepReport>(value).unwrap(), p);

        // An empty report is still a valid shape: every field defaults.
        let value = serde_json::to_value(StepReport::default()).unwrap();
        assert_eq!(value, json!({}));
        assert_eq!(
            serde_json::from_value::<StepReport>(value).unwrap(),
            StepReport::default()
        );
    }

    /// T3 (issue #53): a `thread_started` line written before the runner
    /// existed has neither key; it reads back as two `None`s and
    /// re-serialises byte for byte, so old logs stay replayable.
    #[test]
    fn a_pre_runner_thread_started_line_gains_no_new_fields() {
        let line = json!({
            "project": "vendela",
            "root": "/srv/vendela",
            "created_by": {"kind": "user", "id": "steve"}
        });
        let p: ThreadStartedPayload = serde_json::from_value(line.clone()).unwrap();
        assert_eq!(p.parent_thread, None);
        assert_eq!(p.step, None);
        assert_eq!(serde_json::to_value(&p).unwrap(), line);

        // And a child thread's line carries both, so a replay can tell a
        // step's thread from one a person started.
        let child = ThreadStartedPayload {
            parent_thread: Some(a_child_thread()),
            step: Some("implement".into()),
            ..p
        };
        let value = serde_json::to_value(&child).unwrap();
        assert_eq!(value["parent_thread"], a_child_thread().to_string());
        assert_eq!(value["step"], "implement");
        assert_eq!(
            serde_json::from_value::<ThreadStartedPayload>(value).unwrap(),
            child
        );
    }

    /// T1 (issue #84): `front` is the person's front thread's flag. A
    /// line written before #84 reads as `false`, `false` is never
    /// written (so the bytes are the ones an old line has), and `true`
    /// round-trips.
    #[test]
    fn the_front_flag_round_trips_and_an_old_line_reads_as_false() {
        let old = json!({
            "project": "vendela",
            "root": "/srv/vendela",
            "created_by": {"kind": "user", "id": "steve"}
        });
        let read: ThreadStartedPayload = serde_json::from_value(old.clone()).unwrap();
        assert!(!read.front, "a line without the key is not front");
        // And it writes back byte for byte: `false` is never serialised.
        assert_eq!(serde_json::to_value(&read).unwrap(), old);
        assert!(
            !serde_json::to_value(&read)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("front"),
            "the flag is absent when false"
        );

        // A front thread's line carries the key, and reads back as front.
        let mut front = read.clone();
        front.front = true;
        let value = serde_json::to_value(&front).unwrap();
        assert_eq!(value["front"], json!(true));
        assert_eq!(
            serde_json::from_value::<ThreadStartedPayload>(value).unwrap(),
            front
        );
    }

    /// T5 (issue #96): an `assistant_message` line written before the
    /// finish reason existed reads back with none, and a `None` never
    /// writes the key.
    #[test]
    fn an_old_assistant_message_reads_without_a_finish_reason() {
        let old = json!({
            "blocks": [{"type": "text", "text": "hi"}],
        });
        let read: AssistantMessagePayload = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(read.finish_reason, None);
        assert_eq!(serde_json::to_value(&read).unwrap(), old);
        assert!(
            !serde_json::to_value(&read)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("finish_reason"),
            "no reason is written when there is none"
        );

        // A recorded reason is written and reads back.
        let mut recorded = read.clone();
        recorded.finish_reason = Some("length".to_owned());
        let value = serde_json::to_value(&recorded).unwrap();
        assert_eq!(value["finish_reason"], json!("length"));
        assert_eq!(
            serde_json::from_value::<AssistantMessagePayload>(value).unwrap(),
            recorded
        );
    }
}
