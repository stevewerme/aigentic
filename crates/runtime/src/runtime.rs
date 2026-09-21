use std::time::Duration;

use aigentic_core::{AgentId, Budget, Event, Provider, ToolCall};
use aigentic_log::ThreadLog;
use aigentic_policy::Policy;
use aigentic_skills::SkillSet;
use aigentic_tools::ToolRegistry;

use crate::approver::{Approver, DenyAll};
use crate::layers::Layers;
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
    pub(crate) session_grants: Vec<SessionGrant>,
    pub(crate) skills: SkillSet,
    pub(crate) log: ThreadLog,
    pub(crate) agent: AgentId,
    pub(crate) budget: Budget,
    pub(crate) layers: Layers,
    pub(crate) compaction: CompactionSettings,
    /// Label recorded on summaries; the provider trait has no name.
    pub(crate) model_label: String,
    /// The last call's reported prompt size and the context length it was
    /// measured at, so window fill is exact plus the estimated growth.
    pub(crate) measured: Option<(u64, usize)>,
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
            session_grants: Vec::new(),
            skills: SkillSet::default(),
            log,
            agent,
            budget: DEFAULT_BUDGET,
            layers: Layers::default(),
            compaction: DEFAULT_COMPACTION,
            model_label: "unknown".into(),
            measured: None,
        }
    }

    pub fn with_policy(mut self, policy: Policy) -> Self {
        self.policy = policy;
        self
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
        self
    }

    pub fn layers(&self) -> &Layers {
        &self.layers
    }

    pub fn project(&self) -> Option<&crate::Project> {
        self.layers.project.as_ref()
    }

    /// The prefix for the next call, from the layers and the skills.
    pub(crate) fn prefix(&self) -> crate::Prefix<'_> {
        crate::Prefix {
            global: self.layers.global.instructions.as_deref(),
            project: self.layers.project_instructions(),
            knowledge: None,
            memory: self.layers.memory_prefix(),
            skills: self.skills_prefix(),
        }
    }

    pub fn log(&self) -> &ThreadLog {
        &self.log
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
}

/// How a turn ended. The same information is in the `turn_ended` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnOutcome {
    /// `done`, `max_iterations`, `max_tokens`, `max_wall_time` or
    /// `provider_error`.
    pub reason: String,
    pub iterations: u32,
    pub tokens: u64,
    pub elapsed: Duration,
}
