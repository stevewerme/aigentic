use std::sync::Arc;
use std::time::Duration;

use aigentic_core::{AgentId, Budget, Event, Provider, ToolCall};
use aigentic_log::ThreadLog;
use aigentic_policy::Policy;
use aigentic_skills::SkillSet;
use aigentic_tools::ToolRegistry;

use aigentic_core::{ContentBlock, Message, Role};
use aigentic_tools::{KnowledgeSnapshot, SEARCH_KNOWLEDGE, SearchKnowledgeTool};

use crate::approver::{Approver, DenyAll};
use crate::decisions::{Decisions, Pending};
use crate::knowledge::{Knowledge, KnowledgeMode};
use crate::layers::Layers;
use crate::mode::Mode;
use crate::seams::SessionGrant;

/// Per-turn defaults. `max_tokens` counts every call's input and output
/// over the turn, so a growing context spends it fast: the phase 3
/// acceptance used 225k over eleven iterations on one small ticket.
/// `[profiles.<name>.budget]` in the config overrides any field.
pub const DEFAULT_BUDGET: Budget = Budget {
    max_iterations: 50,
    max_tokens: 2_000_000,
    max_wall_time: Duration::from_secs(1800),
};

/// When and how the runtime compacts. See docs/PLAN-phase2.md section 4.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompactionSettings {
    /// Fraction of `Capabilities::max_context_tokens` that triggers compaction.
    pub trigger_fraction: f32,
    /// Complete turns kept verbatim after a summary.
    pub keep_turns: usize,
    /// Tool results longer than this are truncated first.
    pub max_result_bytes: usize,
    /// Output cap for the summarisation call.
    pub summary_max_output_tokens: u64,
}

pub const DEFAULT_COMPACTION: CompactionSettings = CompactionSettings {
    trigger_fraction: 0.7,
    keep_turns: 8,
    max_result_bytes: 4096,
    summary_max_output_tokens: 2048,
};

/// One thread's runtime: the provider, the tool registry and the single
/// writer for that thread's log.
pub struct Runtime {
    pub(crate) provider: Box<dyn Provider>,
    pub(crate) registry: ToolRegistry,
    pub(crate) policy: Policy,
    pub(crate) approver: Box<dyn Approver>,
    /// Phase 5: when set, an ask parks the turn here instead of calling
    /// the approver.
    pub(crate) decisions: Option<Arc<Decisions>>,
    pub(crate) session_grants: Vec<SessionGrant>,
    /// The permission mode; session state, never persisted.
    pub(crate) mode: Mode,
    pub(crate) skills: SkillSet,
    pub(crate) log: ThreadLog,
    pub(crate) agent: AgentId,
    pub(crate) budget: Budget,
    pub(crate) layers: Layers,
    pub(crate) knowledge: Knowledge,
    pub(crate) knowledge_mode: KnowledgeMode,
    pub(crate) knowledge_snapshot: KnowledgeSnapshot,
    pub(crate) compaction: CompactionSettings,
    /// Label recorded on summaries; the provider trait has no name.
    pub(crate) model_label: String,
    /// The last call's reported prompt size and the context length it was
    /// measured at, so window fill is exact plus the estimated growth.
    pub(crate) measured: Option<(u64, usize)>,
    /// The harness's standing instructions in the prefix; off unless the
    /// builder asks, so the library's own context stays exactly what its
    /// caller put in.
    pub(crate) harness_instructions: Option<&'static str>,
}

impl Runtime {
    /// A runtime with the default policy and no approver: everything
    /// policy would ask about is denied until `with_approver`.
    pub fn new(
        provider: Box<dyn Provider>,
        registry: ToolRegistry,
        log: ThreadLog,
        agent: AgentId,
    ) -> Self {
        Self {
            provider,
            registry,
            policy: Policy::defaults(),
            approver: Box::new(DenyAll),
            decisions: None,
            session_grants: Vec::new(),
            mode: Mode::default(),
            skills: SkillSet::default(),
            log,
            agent,
            budget: DEFAULT_BUDGET,
            layers: Layers::default(),
            knowledge: Knowledge::default(),
            knowledge_mode: KnowledgeMode::Inline,
            knowledge_snapshot: KnowledgeSnapshot::default(),
            compaction: DEFAULT_COMPACTION,
            model_label: "unknown".into(),
            measured: None,
            harness_instructions: None,
        }
    }

    /// Put the harness's standing instructions in the prefix (how to use
    /// `update_tasks`); the daemon does for every thread it builds.
    pub fn with_harness_instructions(mut self) -> Self {
        self.harness_instructions = Some(crate::harness_tools::HARNESS_INSTRUCTIONS);
        self.measured = None;
        self
    }

    pub fn with_policy(mut self, policy: Policy) -> Self {
        self.policy = policy;
        self
    }

    /// Phase 5: permission requests and `ask_human` questions park the
    /// turn on `decisions`, which any approver may answer from anywhere.
    /// The `Approver` is then not consulted.
    pub fn with_decisions(mut self, decisions: Arc<Decisions>) -> Self {
        self.decisions = Some(decisions);
        self
    }

    pub fn decisions(&self) -> Option<&Arc<Decisions>> {
        self.decisions.as_ref()
    }

    pub fn with_approver(mut self, approver: Box<dyn Approver>) -> Self {
        self.approver = approver;
        self
    }

    /// The enabled, hash-verified skills. Their descriptions join the
    /// stable prefix; a change here resets the window measure.
    pub fn with_skills(mut self, skills: SkillSet) -> Self {
        self.skills = skills;
        self.measured = None;
        self
    }

    pub fn skills(&self) -> &SkillSet {
        &self.skills
    }

    /// The prefix block listing enabled skills, `None` when there are none.
    pub(crate) fn skills_prefix(&self) -> Option<String> {
        (!self.skills.is_empty()).then(|| self.skills.descriptions())
    }

    pub fn with_registry(mut self, registry: ToolRegistry) -> Self {
        self.registry = registry;
        self
    }

    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    pub fn registry_mut(&mut self) -> &mut ToolRegistry {
        &mut self.registry
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// The permission mode. Takes effect on the next `policy_check`; a
    /// `Deny` from the rules stands in every mode.
    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The standing `AllowForSession` answers, in the order given.
    pub fn session_grants(&self) -> &[SessionGrant] {
        &self.session_grants
    }

    /// Swap the provider between turns (`/profile`). `label` is the model
    /// name recorded on summaries. The window measure resets and the
    /// knowledge mode is re-decided for the new provider's window, so a
    /// folder that was inline can become an index and back.
    pub fn set_provider(
        &mut self,
        provider: Box<dyn Provider>,
        label: impl Into<String>,
    ) -> Result<(), crate::ProjectError> {
        self.provider = provider;
        self.model_label = label.into();
        self.measured = None;
        self.reload_knowledge()
    }

    /// The model name recorded on summaries: the profile's model.
    pub fn model_label(&self) -> &str {
        &self.model_label
    }

    pub fn with_compaction(mut self, settings: CompactionSettings) -> Self {
        self.compaction = settings;
        self
    }

    /// Recorded on summary compactions so they are auditable.
    pub fn with_model_label(mut self, label: impl Into<String>) -> Self {
        self.model_label = label.into();
        self
    }

    pub fn compaction(&self) -> &CompactionSettings {
        &self.compaction
    }

    pub fn with_budget(mut self, budget: Budget) -> Self {
        self.budget = budget;
        self
    }

    pub fn set_budget(&mut self, budget: Budget) {
        self.budget = budget;
    }

    /// The global and project layers; their instructions and memory join
    /// the stable prefix and their denials narrow the tools the model sees.
    pub fn with_layers(mut self, layers: Layers) -> Self {
        self.layers = layers;
        self.measured = None;
        // Knowledge cannot fail the constructor; an unreadable folder is
        // reported on the first turn by `refresh_knowledge`.
        let _ = self.reload_knowledge();
        self
    }

    /// Read the knowledge folder, count it with the provider, decide the
    /// mode, and register or remove `search_knowledge` accordingly.
    pub fn reload_knowledge(&mut self) -> Result<(), crate::ProjectError> {
        let Some(project) = self.layers.project.as_ref() else {
            return Ok(());
        };
        let dir = project.knowledge_dir();
        let provider = &*self.provider;
        let count = |text: &str| {
            provider.count_tokens(&[Message {
                role: Role::System,
                author: aigentic_core::Author::System,
                blocks: vec![ContentBlock::Text(text.to_owned())],
            }])
        };
        let knowledge = Knowledge::load(&dir, &count)?;
        let window = provider.capabilities().max_context_tokens;
        let threshold = project.file.knowledge.threshold_fraction;
        let max_hits = project.file.knowledge.max_hits;
        let mode = knowledge.mode(window, threshold);
        *self
            .knowledge_snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = knowledge.sections();
        match mode {
            KnowledgeMode::Index if self.registry.get(SEARCH_KNOWLEDGE).is_none() => {
                let tool = SearchKnowledgeTool::new(self.knowledge_snapshot.clone(), max_hits);
                let _ = self.registry.register(Box::new(tool));
            }
            KnowledgeMode::Inline => {
                self.registry.remove(SEARCH_KNOWLEDGE);
            }
            KnowledgeMode::Index => {}
        }
        self.knowledge = knowledge;
        self.knowledge_mode = mode;
        self.measured = None;
        Ok(())
    }

    /// Reload when the folder changed on disk. Called at the start of a
    /// turn, never mid-turn.
    pub(crate) fn refresh_knowledge(&mut self) -> Result<(), crate::ProjectError> {
        let changed = self
            .layers
            .project
            .as_ref()
            .is_some_and(|p| self.knowledge.changed(&p.knowledge_dir()));
        if changed {
            self.reload_knowledge()?;
        }
        Ok(())
    }

    /// Re-read the memory files at the start of a turn, so a hand edit
    /// between turns reaches the next prefix (done-when 4). A change
    /// resets the window measure like any other prefix change.
    pub(crate) fn refresh_memory(&mut self) -> Result<(), crate::ProjectError> {
        let Some(project) = self.layers.project.as_mut() else {
            return Ok(());
        };
        let before = project.memory.clone();
        project.reload_memory()?;
        if project.memory != before {
            self.measured = None;
        }
        Ok(())
    }

    pub fn knowledge(&self) -> &Knowledge {
        &self.knowledge
    }

    /// The window fill for `context`, and the provider's window.
    pub fn window_usage(&self, context: &[Message]) -> WindowUsage {
        WindowUsage {
            tokens_in_window: self.fill(context),
            window: self.provider.capabilities().max_context_tokens,
        }
    }

    pub fn knowledge_mode(&self) -> KnowledgeMode {
        self.knowledge_mode
    }

    pub fn layers(&self) -> &Layers {
        &self.layers
    }

    pub fn project(&self) -> Option<&crate::Project> {
        self.layers.project.as_ref()
    }

    /// For a client that reloads memory after a hand edit; the prefix
    /// measure is reset so the next fill is exact.
    pub fn project_mut(&mut self) -> Option<&mut crate::Project> {
        self.measured = None;
        self.layers.project.as_mut()
    }

    /// The prefix for the next call, from the layers and the skills.
    pub(crate) fn prefix(&self) -> crate::Prefix<'_> {
        crate::Prefix {
            global: self.layers.global.instructions.as_deref(),
            harness: self.harness_instructions,
            project: self.layers.project_instructions(),
            participants: self.participants_line(),
            knowledge: self.knowledge.prefix(self.knowledge_mode),
            memory: self.layers.memory_prefix(),
            skills: self.skills_prefix(),
        }
    }

    /// `Participants in this project: magnus (approve), steve (admin)`,
    /// or nothing when the project names nobody.
    fn participants_line(&self) -> Option<String> {
        let listed = self.layers.project.as_ref()?.file.participants.describe();
        (!listed.is_empty()).then(|| format!("Participants in this project: {}", listed.join(", ")))
    }

    pub fn log(&self) -> &ThreadLog {
        &self.log
    }

    /// Whether the log's last event is a `turn_ended` with reason
    /// `asked_human`: the human's answer is recorded and the model has not
    /// yet seen it, so the client should `continue_turn`.
    pub fn awaiting_continuation(&self) -> Result<bool, crate::RuntimeError> {
        let events = self.log.read_all()?;
        Ok(events.last().is_some_and(|e| {
            e.kind == aigentic_core::EventKind::TurnEnded
                && serde_json::from_value::<aigentic_log::TurnEndedPayload>(e.payload.clone())
                    .is_ok_and(|p| p.reason == ASKED_HUMAN)
        }))
    }

    pub fn budget(&self) -> &Budget {
        &self.budget
    }
}

/// What a client sees while a turn runs. Everything durable is also an
/// `Event`; the deltas exist only so text can be shown as it streams.
#[derive(Debug)]
pub enum Signal<'a> {
    TextDelta(&'a str),
    ToolCallStarted(&'a ToolCall),
    Event(&'a Event),
    /// After every model call: the window fill, for a status line
    /// (phase 6). The daemon computes it; a client never counts.
    Usage(WindowUsage),
    /// A remark for the person that is not an event (a rules file that
    /// could not be written).
    Note(String),
    /// The turn parked on a decision (phase 5); a client shows what is
    /// waited for.
    Waiting(&'a Pending),
}

/// How full the model's window is: the last call's reported prompt size
/// plus what was appended since, against the provider's context length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowUsage {
    pub tokens_in_window: u64,
    pub window: u64,
}

/// The `turn_ended` reason when a participant interrupted the turn; an
/// `interrupted` event naming them precedes it.
pub const INTERRUPTED: &str = "interrupted";

/// The `turn_ended` reason when a human answered `ask_human`: the answer
/// is in the log as the call's result, and the client continues with
/// `continue_turn`, a new turn with a fresh budget.
pub const ASKED_HUMAN: &str = "asked_human";

/// How a turn ended. The same information is in the `turn_ended` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnOutcome {
    /// `done`, `max_iterations`, `max_tokens`, `max_wall_time` or
    /// `provider_error`.
    pub reason: String,
    pub iterations: u32,
    pub tokens: u64,
    pub elapsed: Duration,
    /// Files written or edited without error this turn; see
    /// `TurnEndedPayload::touched`.
    pub touched: Vec<String>,
}
