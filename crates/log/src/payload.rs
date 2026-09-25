//! Kind-specific payload shapes. `Event.payload` is free JSON on the wire;
//! these structs are the contract for what each kind carries.

use std::path::PathBuf;

use aigentic_core::{Author, ContentBlock, RiskClass, ToolCall, ToolResult};
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadRenamedPayload {
    pub title: String,
}

/// Payload of a `thread_started` event, the first event of a thread the
/// daemon created: the project it belongs to (`None` outside any) and
/// the root its tools run in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadStartedPayload {
    pub project: Option<String>,
    pub root: PathBuf,
    pub created_by: Author,
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
    /// `"done"`, `"resumed"`, `"asked_human"`, the budget that was hit,
    /// or `provider_error: ...`.
    pub reason: String,
    /// The `path` of every `write_file` and `edit_file` call in the turn
    /// that returned without error, in order, each once; so a stop on a
    /// budget can say which files it was in. Empty on lines written before
    /// phase 4 step 9.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub touched: Vec<String>,
}

impl TurnEndedPayload {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            touched: Vec::new(),
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
        usage: Usage,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextEvictedPayload {
    /// The last stubbed event's seq, inclusive.
    pub through_seq: u64,
}

/// Payload of a `provider_retried` event (issue #31): which retry is
/// starting, out of how many, why the call is quiet, and how long the
/// backoff waits before the next attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRetriedPayload {
    /// 1-based: the retry that is starting.
    pub attempt: u32,
    /// Total retries this call will make.
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    fn phase0_tool_result_lines_read_back_without_policy() {
        let line = json!({"id": "c1", "content": "ok", "is_error": false});
        let p: ToolResultPayload = serde_json::from_value(line).unwrap();
        assert_eq!(p.result.id, "c1");
        assert_eq!(p.policy, None);
    }

    #[test]
    fn context_evicted_round_trips_with_one_field() {
        let p = ContextEvictedPayload { through_seq: 41 };
        let value = serde_json::to_value(&p).unwrap();
        assert_eq!(value, json!({"through_seq": 41}));
        assert_eq!(
            serde_json::from_value::<ContextEvictedPayload>(value).unwrap(),
            p
        );
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
}
