use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use aigentic_core::{AgentId, Budget, Event, Provider, ToolCall};
use aigentic_log::ThreadLog;
use aigentic_policy::{Policy, StepOverlay};
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
/// over the turn, cache reads at `cache_read_price_ratio` (a quarter by
/// default), so a growing uncached context spends it fast: the phase 3
/// acceptance used 225k over eleven iterations on one small ticket.
/// `[profiles.<name>.budget]` in the config overrides any field.
pub const DEFAULT_BUDGET: Budget = Budget {
    max_iterations: 50,
    max_tokens: 2_000_000,
    max_wall_time: Duration::from_secs(1800),
    cache_read_price_ratio: 0.25,
};

/// What a profile's tokens cost, in USD per 1M tokens. Kept in the
/// runtime rather than derived here so a log line can carry the price of
/// the call that was made, whatever the config says later.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Prices {
    pub input: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub output: f64,
}

impl Prices {
    /// What one call cost, cache kinds at their own rates.
    pub fn cost_usd(&self, u: &aigentic_log::Usage) -> f64 {
        (self.input * u.input_tokens as f64
            + self.cache_read * u.cache_read_tokens as f64
            + self.cache_write * u.cache_write_tokens as f64
            + self.output * u.output_tokens as f64)
            / 1_000_000.0
    }
}

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
    /// Calls of the running turn kept in full before in-turn eviction
    /// stubs the rest (issue #30). Failed results and the last result of
    /// each distinct tool always stay on top of these. Read only when
    /// `working_set_tokens = 0`: with a target set the sweep's depth
    /// is the floor instead — all but the last `EVICT_BLOCK_CALLS` calls —
    /// so this has no effect.
    pub keep_last_calls: usize,
    /// The working-set target (issue #76): the tokens one call's context
    /// aims at, whatever the model's window. Compaction's trigger line,
    /// the closed-turn batch and the in-turn sweep all aim under it
    /// (issue #30). Zero means no target, only the fraction.
    pub working_set_tokens: u64,
    /// The in-turn sweep runs only while the context is over this line
    /// (issue #32): under it nothing is stubbed, so a turn that is still
    /// reading keeps what it read. Zero sweeps on call count alone.
    pub evict_above_tokens: u64,
    /// The share, in percent, of `working_set_tokens` a sweep or a batch
    /// must free to be worth breaking the cached prefix for (issue #35):
    /// a quarter, 30k at the 120k default. A sweep frees only the
    /// material between the current boundary and the floor, so this is
    /// what spaces the sweeps. Zero moves the boundary as soon as the
    /// floor advances, however little that frees.
    pub evict_min_free_percent: u32,
}

pub const DEFAULT_COMPACTION: CompactionSettings = CompactionSettings {
    trigger_fraction: 0.7,
    keep_turns: 8,
    max_result_bytes: 4096,
    summary_max_output_tokens: 2048,
    keep_last_calls: 12,
    working_set_tokens: 120_000,
    evict_above_tokens: 64_000,
    evict_min_free_percent: crate::evict::EVICT_MIN_FREE_PERCENT,
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
    /// Per-step context when this thread is a step thread (issue #55):
    /// the step's name in the group and its deny overlay. Held here, not
    /// on `Policy`, so `set_project` or a whole `Policy` swap cannot
    /// drop it (`## Plan amendment 2` item 2).
    pub(crate) step: Option<StepContext>,
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
    /// The profile and its prices, when the config sets them: stamped on
    /// every `usage` line so a thread's spend is in the log itself
    /// (issue #31).
    pub(crate) profile: Option<String>,
    /// The profile's effort label, for the wire and for each `usage`
    /// line (issues #43, #44).
    pub(crate) effort: Option<String>,
    pub(crate) prices: Option<Prices>,
    /// The last call's reported prompt size and the context length it was
    /// measured at, so window fill is exact plus the estimated growth.
    pub(crate) measured: Option<(u64, usize)>,
    /// The calibration between the estimator and the provider's own count
    /// (issue #52): a smoothed `reported / (estimate + overhead)`, seeded
    /// at 1.0 and learned from every call that reported a usage. It never
    /// resets with the prefix — the tokenizer relation does not change
    /// with it — only where the provider does.
    pub(crate) ratio: f64,
    /// What this thread's tool schemas cost on the wire, at the
    /// estimator's rate (issue #52): the part of a request
    /// `estimate_tokens` does not cover, so a probe can be priced as
    /// `ratio · (estimate + overhead)`. Measured once per turn from the
    /// specs the turn sends, so a registry change is picked up.
    pub(crate) overhead: u64,
    /// The whole thread, uncalibrated (issue #99): the raw estimate of the
    /// conversation in the log, as if nothing had been stubbed, swept or
    /// summarised. Seeded once in `new` over the log as opened, then
    /// stepped by each later append with that message's own estimate —
    /// through both write paths, `Runtime::append` and `append_queued`.
    ///
    /// The bound: at any moment with no call awaiting its result this sum
    /// equals the full recount `thread_baseline` performs then, up to the
    /// estimator's per-message rounding.
    /// Mid-call it may be ahead by the messages the projection holds back
    /// between a call and its result. It never decreases: a sweep, a batch
    /// and a summary are skipped, and reasoning blobs are excluded from
    /// every message, so `turn_ended` cannot shrink it either.
    ///
    /// Stored raw and calibrated at read time in `window_usage`, so a
    /// ratio change re-prices the whole thread with one rounding.
    pub(crate) thread_raw: u64,
    /// The harness's standing instructions in the prefix; off unless the
    /// builder asks, so the library's own context stays exactly what its
    /// caller put in.
    pub(crate) harness_instructions: Option<&'static str>,
    /// The daemon's listing of the projects in reach (issue #81), filled
    /// for a thread a person created and rendered into the prefix after
    /// `participants`. The runtime cannot compute it: only the daemon
    /// knows every project and which workspace each is in. `None` is no
    /// block at all, which is what the library and every test keep.
    pub(crate) projects: Option<String>,
    /// The provider for side jobs (titles, memory extraction): the
    /// config's `utility_profile` when set, else the thread's own.
    pub(crate) utility: Option<Box<dyn Provider>>,
    /// The utility profile's model, set with its provider; recorded on
    /// `memory_extracted`. `None` means no utility is configured and the
    /// thread's own `model_label` ran the extraction (issue #18).
    pub(crate) utility_label: Option<String>,
    /// What the utility profile's calls cost, stamped on a
    /// `memory_extracted` usage line (issue #46). The utility provider
    /// is a different endpoint from the thread's, so only its own table
    /// prices its calls: when it has none the line stays unpriced, on a
    /// model name the report can price later. The thread's prices apply
    /// only when the thread's provider ran the extraction, i.e. when no
    /// utility is configured at all.
    pub(crate) utility_prices: Option<Prices>,
    /// The turn clock (issue #47): one monotonic and one wall reading,
    /// taken together. Both go through this seam so a test can script a
    /// sleep the way the machine makes one — wall time moving on while
    /// running time stands still.
    pub(crate) clock: Arc<dyn Fn() -> (Instant, SystemTime) + Send + Sync>,
    /// The keep-awake guard's status reader, installed by the daemon
    /// (issue #47). Read when a turn ends, never earlier, so a guard
    /// that failed to spawn mid-run is reported by the turn it affected
    /// rather than freezing its status into every thread at startup.
    pub(crate) keep_awake: Option<Arc<dyn Fn() -> Option<String> + Send + Sync>>,
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
        // The thread figure is seeded once, here, over the log as opened:
        // this is the only constructor of a `Runtime` from a log. Every
        // later append steps it (issue #99).
        let thread_raw = crate::support::thread_baseline(&*provider, &log);
        Self {
            provider,
            registry,
            policy: Policy::defaults(),
            approver: Box::new(DenyAll),
            decisions: None,
            session_grants: Vec::new(),
            mode: Mode::default(),
            step: None,
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
            profile: None,
            effort: None,
            prices: None,
            measured: None,
            ratio: 1.0,
            overhead: 0,
            thread_raw,
            harness_instructions: None,
            projects: None,
            utility: None,
            utility_label: None,
            utility_prices: None,
            clock: Arc::new(|| (Instant::now(), SystemTime::now())),
            keep_awake: None,
        }
    }

    /// Move the thread to another project: every project-derived part is
    /// replaced (layers, policy, skills, tools and their working root,
    /// provider), session grants end, knowledge reloads, and a
    /// `project_switched` event by `by` records it. Pins, compaction and
    /// the transcript stay: they are the thread's, in the log.
    pub fn set_project(
        &mut self,
        ctx: ProjectContext,
        by: aigentic_core::Author,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<(), crate::RuntimeError> {
        // A step thread keeps its overlay for its whole life (issue #55,
        // `## Plan amendment 2` item 2). `set_project` swaps the policy
        // wholesale, and with it the rules the overlay stands in front
        // of, so a step thread refuses the move rather than run with an
        // overlay over a policy nobody chose.
        if let Some(step) = &self.step {
            return Err(crate::RuntimeError::StepThread(format!(
                "`{}` is a step thread and cannot switch project",
                step.name
            )));
        }
        let from = self.layers.project.as_ref().map(|p| p.name.clone());
        let payload = aigentic_log::ProjectSwitchedPayload {
            from,
            to: ctx.name.clone(),
            root: ctx.root.clone(),
            workspace: ctx.workspace.clone(),
        };
        self.layers = ctx.layers;
        self.policy = ctx.policy;
        self.skills = ctx.skills;
        self.registry = ctx.registry;
        self.provider = ctx.provider;
        self.model_label = ctx.model_label;
        self.projects = ctx.projects;
        self.profile = ctx.profile;
        self.effort = ctx.effort;
        self.prices = ctx.prices;
        self.session_grants.clear();
        self.measured = None;
        // Another project is another provider and another tool registry.
        self.ratio = 1.0;
        self.overhead = 0;
        if self.layers.project.is_none() {
            self.knowledge = Knowledge::default();
        }
        self.reload_knowledge()?;
        self.append(
            aigentic_core::EventKind::ProjectSwitched,
            by,
            serde_json::to_value(payload).expect("serialisable"),
            None,
            observe,
        )?;
        Ok(())
    }

    /// The project the thread is in, by name, when it is in one: what
    /// `suggest_project` compares a proposal against (issue #7).
    pub fn current_project(&self) -> Option<String> {
        self.layers.project.as_ref().map(|p| p.name.clone())
    }

    /// Append a `decision_answered` event (#74): `by` answered, about
    /// the proposal `parent`. `correction` only with `Corrected`; `note`
    /// when the answering path has words of its own (`no one to
    /// answer`, `turn interrupted`, `switch failed: …`).
    pub(crate) fn append_decision_answered(
        &mut self,
        answer: aigentic_log::DecisionAnswer,
        correction: Option<String>,
        note: Option<String>,
        by: aigentic_core::Author,
        parent: ulid::Ulid,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<(), crate::RuntimeError> {
        let payload = aigentic_log::DecisionAnsweredPayload {
            answer,
            correction,
            note,
        };
        self.append(
            aigentic_core::EventKind::DecisionAnswered,
            by,
            serde_json::to_value(payload).expect("serialisable"),
            Some(parent),
            observe,
        )?;
        Ok(())
    }

    /// A smaller model for side jobs (phase 6 step 9). `label` is its
    /// model name, recorded on `memory_extracted` so the log names the
    /// provider that ran the extraction (issue #18); `prices` is that
    /// profile's `[prices]`, stamped on the extraction's usage line
    /// (issue #46).
    pub fn with_utility(
        mut self,
        provider: Box<dyn Provider>,
        label: impl Into<String>,
        prices: Option<Prices>,
    ) -> Self {
        self.utility = Some(provider);
        self.utility_label = Some(label.into());
        self.utility_prices = prices;
        self
    }

    /// The provider side jobs use.
    pub fn utility(&self) -> &dyn Provider {
        self.utility.as_deref().unwrap_or(self.provider.as_ref())
    }

    /// The utility profile's model, when one is configured; `None` means
    /// side jobs run on the thread's own `model_label`.
    pub fn utility_label(&self) -> Option<&str> {
        self.utility_label.as_deref()
    }

    /// The title from the log's last `thread_renamed`, if any.
    pub fn title(&self) -> Result<Option<String>, crate::RuntimeError> {
        Ok(crate::title::title_of(self.log.events()))
    }

    /// Set the title: a `thread_renamed` event by `author`, with no call
    /// behind it (a person's `/rename`).
    pub fn rename(
        &mut self,
        author: aigentic_core::Author,
        title: &str,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<(), crate::RuntimeError> {
        self.rename_with(author, title, None, None, observe)
    }

    /// Set the title with the utility call that proposed it: the
    /// `model` and the call's `usage`, `cost_usd` stamped from the
    /// utility profile's `[prices]` by the same rule as a memory
    /// extraction (issues #46, #49). Both absent when no call stands
    /// behind the line, and absent prices claim nothing about money.
    pub fn rename_with(
        &mut self,
        author: aigentic_core::Author,
        title: &str,
        model: Option<String>,
        usage: Option<aigentic_core::Usage>,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<(), crate::RuntimeError> {
        let title = crate::title::clean(title);
        if title.is_empty() {
            return Ok(());
        }
        let usage = usage.map(|u| {
            let mut usage = aigentic_log::Usage::reported(u);
            usage.cost_usd = self.utility_prices.map(|p| p.cost_usd(&usage));
            usage
        });
        let payload = serde_json::to_value(aigentic_log::ThreadRenamedPayload {
            title,
            model,
            usage,
        })
        .expect("serialisable");
        self.append(
            aigentic_core::EventKind::ThreadRenamed,
            author,
            payload,
            None,
            observe,
        )?;
        Ok(())
    }

    /// After a finished first turn and while there is no title: ask the
    /// utility model for one and record it by `system`. `Ok(None)` when
    /// there is nothing to do, or no utility model is configured.
    pub async fn title_if_untitled(
        &mut self,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<Option<String>, crate::RuntimeError> {
        // Only on a configured utility model: a title is not worth a
        // call on the thread's large model.
        let Some(utility) = self.utility.as_deref() else {
            return Ok(None);
        };
        let model = self
            .utility_label
            .clone()
            .unwrap_or_else(|| self.model_label.clone());
        let (title, usage) = {
            let events = self.log.events();
            if crate::title::title_of(events).is_some() || !crate::title::has_finished_turn(events)
            {
                return Ok(None);
            }
            crate::title::propose_title(utility, events).await?
        };
        if title.is_empty() {
            return Ok(None);
        }
        // Stamped like a memory extraction (issues #46, #49) so `/cost`
        // and the stats footer count what the title cost.
        self.rename_with(
            aigentic_core::Author::System,
            &title,
            Some(model),
            Some(usage),
            observe,
        )?;
        Ok(Some(title))
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

    /// Make this thread a step thread (issue #55): `step` is its name in
    /// the group, `deny` the workflow's deny list. The list is parsed
    /// once, here, so a typo fails where the thread is built rather than
    /// at the first call. The overlay stands *before* the rules, so no
    /// project rule or mode can talk a step past it.
    pub fn with_step(
        mut self,
        step: impl Into<String>,
        deny: &[String],
    ) -> Result<Self, aigentic_policy::DenyParseError> {
        let overlay = StepOverlay::parse(deny)?;
        self.step = Some(StepContext {
            name: step.into(),
            overlay,
        });
        Ok(self)
    }

    /// This thread's step name, `None` for a thread that is not a step.
    pub fn step(&self) -> Option<&str> {
        self.step.as_ref().map(|s| s.name.as_str())
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
        profile: Option<String>,
        prices: Option<Prices>,
    ) -> Result<(), crate::ProjectError> {
        self.provider = provider;
        self.model_label = label.into();
        self.profile = profile;
        self.prices = prices;
        self.measured = None;
        // A different model tokenizes differently: the old ratio is about
        // the old one, so the next calls learn this one from scratch.
        self.ratio = 1.0;
        self.overhead = 0;
        self.reload_knowledge()
    }

    /// The model name recorded on summaries: the profile's model.
    pub fn model_label(&self) -> &str {
        &self.model_label
    }

    /// The profile the thread's provider came from, and its effort
    /// label, for a client's footer (issue #43). `None` when the config
    /// sets neither.
    pub fn identity(&self) -> (Option<String>, String, Option<String>) {
        (
            self.profile.clone(),
            self.model_label.clone(),
            self.effort.clone(),
        )
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

    /// The profile and price table stamped on every `usage` line
    /// (issue #31). Absent prices leave `cost_usd` unset, which is what
    /// a thread on an unpriced endpoint should show.
    pub fn with_pricing(mut self, profile: impl Into<String>, prices: Option<Prices>) -> Self {
        self.set_pricing(profile, prices);
        self
    }

    /// `with_pricing` in place, for a caller that already owns a
    /// `&mut Runtime` (a `Drop` type, a live session).
    pub fn set_pricing(&mut self, profile: impl Into<String>, prices: Option<Prices>) {
        self.profile = Some(profile.into());
        self.prices = prices;
    }

    /// The profile's effort label, stamped on every `usage` line and
    /// named to a client's footer (issues #43, #44).
    pub fn with_effort(mut self, effort: Option<String>) -> Self {
        self.set_effort(effort);
        self
    }

    /// `with_effort` in place, for a caller that already owns a
    /// `&mut Runtime` (a test fixture, a live session).
    pub fn set_effort(&mut self, effort: Option<String>) {
        self.effort = effort;
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

    /// Replace the turn clock (issue #47): both readings are taken
    /// through this seam, so a test can script a sleep the way the
    /// machine makes one — wall time moving on while running time stands
    /// still.
    pub fn with_clock(
        mut self,
        clock: Arc<dyn Fn() -> (Instant, SystemTime) + Send + Sync>,
    ) -> Self {
        self.clock = clock;
        self
    }

    /// Install the keep-awake guard's status reader (issue #47). The
    /// daemon passes a closure over its guard; it is called once when a
    /// turn ends, so a guard that failed to spawn later in the run is
    /// still reported by the turn it affected.
    pub fn with_keep_awake(
        mut self,
        reader: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    ) -> Self {
        self.set_keep_awake(reader);
        self
    }

    /// The same, on a runtime already built: what the daemon's actor
    /// uses, since its runtime lives behind `&mut self` by then.
    pub fn set_keep_awake(&mut self, reader: Arc<dyn Fn() -> Option<String> + Send + Sync>) {
        self.keep_awake = Some(reader);
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

    /// The daemon's listing of the projects in reach (issue #81), which
    /// `ThreadTable` renders and hands to a first build. A runtime built
    /// without it carries no projects block at all.
    pub fn with_projects(mut self, projects: Option<String>) -> Self {
        self.projects = projects;
        self.measured = None;
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

    /// The window fill for `context`, against the ceiling the client
    /// shows: compaction's line, not the provider's full length (the
    /// number that matters since #30 — how close compaction is) — plus
    /// the whole thread (issue #99).
    pub fn window_usage(&self, context: &[Message]) -> WindowUsage {
        WindowUsage {
            tokens_in_window: self.fill(context),
            window: self.window_line(),
            // Priced from the stored raw sum at read time, so a ratio
            // change re-prices the whole thread with one rounding.
            thread_tokens: crate::evict::calibrated_delta(self.thread_raw, self.ratio),
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
            workspace: self.layers.workspace_instructions(),
            project: self.layers.project_instructions(),
            participants: self.participants_line(),
            projects: self.projects.clone(),
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

    /// A full recount of the thread figure, uncalibrated (issue #99): the
    /// seeded baseline recomputed over the log as it stands now. Doc
    /// hidden — it exists so a test can hold the running sum against the
    /// definition it maintains, not as an API.
    #[doc(hidden)]
    pub fn raw_thread_tokens(&self) -> u64 {
        crate::support::thread_baseline(&*self.provider, &self.log)
    }

    /// Whether the log's last event is a `turn_ended` with reason
    /// `asked_human`: the human's answer is recorded and the model has not
    /// yet seen it, so the client should `continue_turn`.
    pub fn awaiting_continuation(&self) -> Result<bool, crate::RuntimeError> {
        let events = self.log.events();
        Ok(events.last().is_some_and(|e| {
            e.kind == aigentic_core::EventKind::TurnEnded
                && serde_json::from_value::<aigentic_log::TurnEndedPayload>(e.payload.clone())
                    .is_ok_and(|p| p.reason == ASKED_HUMAN)
        }))
    }

    /// Whether the current turn already reported (## Plan amendment 2
    /// item 1): a `step_reported` event later than the latest
    /// `user_message`. Derived from the log, so a send-back — a new turn
    /// whose user message follows the report — can report again, while a
    /// second call in one turn cannot.
    pub(crate) fn reported_this_turn(&self) -> bool {
        let events = self.log.events();
        let turn_start = events
            .iter()
            .rev()
            .find(|e| e.kind == aigentic_core::EventKind::UserMessage)
            .map(|e| e.seq);
        events
            .iter()
            .any(|e| e.kind == aigentic_core::EventKind::StepReported && turn_start < Some(e.seq))
    }

    pub fn budget(&self) -> &Budget {
        &self.budget
    }
}

/// A step thread's context (issue #55): the step's name in the group,
/// and the deny overlay the runtime asks before the rules. It is session
/// state and never persisted: the step's own log holds what it reported.
#[derive(Debug, Clone)]
pub struct StepContext {
    pub(crate) name: String,
    pub(crate) overlay: StepOverlay,
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

/// What a thread takes from its project, built by the daemon and swapped
/// in whole by `Runtime::set_project` (phase 6 step 10).
pub struct ProjectContext {
    pub name: Option<String>,
    pub workspace: Option<String>,
    pub root: std::path::PathBuf,
    pub layers: Layers,
    pub policy: Policy,
    pub skills: SkillSet,
    /// Built-in tools rooted at the new root, and its MCP servers.
    pub registry: ToolRegistry,
    pub provider: Box<dyn Provider>,
    pub model_label: String,
    /// The projects in reach for this thread (issue #81), rendered by the
    /// daemon's `ThreadTable`; `None` when the caller has no listing,
    /// which is every caller that is not the daemon.
    pub projects: Option<String>,
    pub profile: Option<String>,
    /// The profile's reasoning effort, when it sets one. Carried to the
    /// wire so a client's footer can name it (issue #43); the runtime
    /// itself does not act on it.
    pub effort: Option<String>,
    pub prices: Option<Prices>,
}

impl std::fmt::Debug for ProjectContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectContext")
            .field("name", &self.name)
            .field("workspace", &self.workspace)
            .field("root", &self.root)
            .field("model_label", &self.model_label)
            .finish_non_exhaustive()
    }
}

/// How full the model's window is: the last call's reported prompt size
/// plus what was appended since, against compaction's line (issue #21:
/// the ceiling a client shows, not the provider's raw length).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowUsage {
    /// What the model sees this call: the fill of the window the context
    /// has to fit in.
    pub tokens_in_window: u64,
    /// Compaction's line, the ceiling `tokens_in_window` is against.
    pub window: u64,
    /// The whole thread (issue #99): the raw estimate of everything the
    /// log holds, as if nothing had been stubbed, swept or summarised,
    /// calibrated by the estimator's `ratio`. "How big the conversation
    /// really is", where `tokens_in_window` is "what the model sees".
    ///
    /// With no call awaiting its result it equals a full recount of the
    /// thread; while a call is in flight it may be ahead by the messages
    /// the projection holds back between that call and its result. It
    /// never decreases — a sweep, a batch, a summary and a `turn_ended`'s
    /// dropped reasoning all leave it alone.
    ///
    /// A short thread can report `tokens_in_window` (the prefix, the tool
    /// schemas and the projection are all in it) larger than this: it is
    /// information, not a warning.
    pub thread_tokens: u64,
}

/// The `turn_ended` reason when a participant interrupted the turn; an
/// `interrupted` event naming them precedes it.
pub const INTERRUPTED: &str = "interrupted";

/// The `turn_ended` reason when a human answered `ask_human`: the answer
/// is in the log as the call's result, and the client continues with
/// `continue_turn`, a new turn with a fresh budget.
pub const ASKED_HUMAN: &str = "asked_human";

/// The `turn_ended` reason when a step called `finish_step` (issue #55):
/// the step reported, so the runner's `step_finished` has something to
/// read. A reason of its own rather than `done`, so the runner's
/// bookkeeping can tell a report from a turn that simply stopped.
pub const STEP_REPORTED: &str = "step_reported";

/// The `turn_ended` reason when the model's own output limit stopped the
/// reply (issue #96). The provider says `length` or `max_tokens`; the
/// turn ends `length`, and no tool call of that reply runs, because the
/// arguments arrived mid-sentence. Distinct from our budget stop, whose
/// reason stays `max_tokens`: the model ran out of room for this reply,
/// not the turn out of tokens.
pub const LENGTH_STOP: &str = "length";

/// The provider's other name for the same stop (issue #96).
pub const MAX_TOKENS_STOP: &str = "max_tokens";

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
