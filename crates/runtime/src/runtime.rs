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
    /// The workspace's knowledge, inline or indexed; `None` when the
    /// thread has no workspace or it has no knowledge.
    pub(crate) workspace_knowledge_mode: Option<KnowledgeMode>,
    /// What `search_knowledge` may reach beyond the snapshot: the thread's
    /// own memory, the understood siblings, the workspace. The tool holds
    /// the same handle, so installing scopes also reaches the tool.
    pub(crate) sources: Arc<crate::knowledge::ScopeSources>,
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
    /// The reach rows `projects` was rendered from (issue #123), which
    /// `read_brief` looks a project's root up in. The block stays the
    /// daemon's string; these carry only what a lookup needs.
    pub(crate) project_rows: Vec<ProjectRow>,
    /// This project's brief (issue #123), read live at build and switch
    /// and re-read at the start of a turn. `None` is no block.
    pub(crate) project_brief: Option<String>,
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
    /// How long a continuous summary's utility call may take (issue #98).
    /// The call runs inside the turn, and the loop checks the wall budget
    /// at its next iteration: the limit bounds how far a wedged utility
    /// endpoint can push a turn past that budget. A timed-out link is
    /// skipped, never fatal. Defaults to [`SUMMARY_LIMIT`], and
    /// [`Runtime::with_summary_limit`] exists for tests: there is no
    /// config key.
    pub(crate) summary_limit: Duration,
}

/// How long a continuous summary's utility call may take (issue #98),
/// the same 60 s the daemon's side jobs get.
pub const SUMMARY_LIMIT: Duration = Duration::from_secs(60);

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
            workspace_knowledge_mode: None,
            sources: Arc::new(crate::knowledge::ScopeSources::new()),
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
            project_rows: Vec::new(),
            project_brief: None,
            utility: None,
            utility_label: None,
            utility_prices: None,
            clock: Arc::new(|| (Instant::now(), SystemTime::now())),
            keep_awake: None,
            summary_limit: SUMMARY_LIMIT,
        }
    }

    /// How long a continuous summary's utility call may take (issue
    /// #98), test-only: there is no config key. A real run uses
    /// [`SUMMARY_LIMIT`].
    #[doc(hidden)]
    pub fn with_summary_limit(mut self, limit: Duration) -> Self {
        self.summary_limit = limit;
        self
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
        self.project_brief = self.layers.project.as_ref().and_then(crate::Project::brief);
        self.policy = ctx.policy;
        self.skills = ctx.skills;
        self.registry = ctx.registry;
        self.provider = ctx.provider;
        self.model_label = ctx.model_label;
        // The reach moves with the project (issue #81, #123): the new
        // project has its own listing and its own siblings' briefs.
        self.projects = ctx.projects;
        self.project_rows = ctx.project_rows;
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
        self.project_brief = self.layers.project.as_ref().and_then(crate::Project::brief);
        self.measured = None;
        // Knowledge cannot fail the constructor; an unreadable folder is
        // reported on the first turn by `refresh_knowledge`.
        let _ = self.reload_knowledge();
        self
    }

    /// The daemon's listing of the projects in reach (issue #81), which
    /// `ThreadTable` renders and hands to a first build, and the rows it
    /// was rendered from (issue #123), which `read_brief` looks a
    /// project's root up in. A runtime built without them carries no
    /// projects block and can read no sibling's brief.
    pub fn with_projects(mut self, projects: Option<String>, rows: Vec<ProjectRow>) -> Self {
        self.projects = projects;
        self.project_rows = rows;
        self.measured = None;
        // The rows arrive after `with_layers` (the daemon builds the
        // layers first), so the resolver and registration catch up here:
        // a sibling's knowledge must be reachable from the next call.
        self.install_scopes();
        let max_hits = self
            .layers
            .project
            .as_ref()
            .map(|p| p.file.knowledge.max_hits);
        if let Some(max_hits) = max_hits {
            self.measure_registration(max_hits);
        }
        self
    }

    /// Read the knowledge folders, count them with the provider, decide
    /// the modes, and register or remove `search_knowledge` accordingly.
    /// The project's folder is read first, so its mode decides how much
    /// room the workspace's knowledge has.
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
        self.knowledge = knowledge;
        self.knowledge_mode = mode;

        // The workspace's knowledge, reloaded when its folder changes.
        // The whole shared dir is the workspace's own, so it is read
        // without a boundary walk.
        let mut workspace_mode = None;
        if let Some(workspace) = self.layers.workspace.as_mut() {
            let ws_dir = workspace.knowledge_dir();
            let stale = match (workspace.knowledge.as_ref(), ws_dir.as_deref()) {
                (Some(loaded), Some(dir)) => loaded.changed(dir),
                (None, Some(dir)) => crate::knowledge::holds_markdown(dir),
                (_, None) => false,
            };
            if stale {
                workspace.knowledge = match ws_dir.as_deref() {
                    Some(dir) => {
                        let loaded = Knowledge::load(dir, &count)?;
                        (!loaded.is_empty()).then_some(loaded)
                    }
                    None => None,
                };
            }
            // The project's mode is decided first; the workspace only
            // gets what the project's inline block left of the line.
            let taken = match mode {
                KnowledgeMode::Inline => self.knowledge.tokens,
                KnowledgeMode::Index => 0,
            };
            let room = (window as f64 * f64::from(threshold)).round() as u64 - taken;
            workspace_mode = workspace.knowledge.as_ref().map(|k| {
                if k.tokens <= room {
                    KnowledgeMode::Inline
                } else {
                    KnowledgeMode::Index
                }
            });
        }
        self.workspace_knowledge_mode = workspace_mode;

        self.install_scopes();
        self.measure_registration(max_hits);
        self.measured = None;
        Ok(())
    }

    /// Tell the resolver which scopes this thread may search, and whether
    /// any of them has anything: the rows and layers as they stand now.
    fn install_scopes(&mut self) {
        self.sources.install(
            self.layers.project.as_ref(),
            &self.knowledge.sections(),
            &self.project_rows,
            self.layers.workspace.as_ref(),
        );
    }

    /// Offer `search_knowledge` when the thread's own knowledge is
    /// indexed or something it may search has knowledge or memory, and
    /// take it away when neither holds.
    fn measure_registration(&mut self, max_hits: usize) {
        let indexed = self.knowledge_mode == KnowledgeMode::Index;
        if indexed || self.sources.has_reach() {
            if self.registry.get(SEARCH_KNOWLEDGE).is_none() {
                let tool = SearchKnowledgeTool::new(self.knowledge_snapshot.clone(), max_hits)
                    .with_sources(self.sources.clone());
                let _ = self.registry.register(Box::new(tool));
            }
        } else {
            self.registry.remove(SEARCH_KNOWLEDGE);
        }
    }

    /// Re-read this project's brief at the start of a turn (issue
    /// #123), so a hand edit between turns reaches the next prefix. An
    /// unchanged read does not reload: like `refresh_memory`, a change
    /// resets the window measure, and nothing else does.
    pub(crate) fn refresh_brief(&mut self) {
        let Some(project) = self.layers.project.as_ref() else {
            return;
        };
        let brief = project.brief();
        if brief != self.project_brief {
            self.project_brief = brief;
            self.measured = None;
        }
    }

    /// Whether `read_brief` is in the tool list the model sees (issue
    /// #123): some project in reach other than this thread's own carries
    /// a brief. A project's own brief is no reason to offer it — that
    /// brief is already inline. Only an understood project offers it: a
    /// row in another workspace is listed but refused. Whether the
    /// layers let the tool through is `tool_visible`'s, checked where
    /// every other spec is.
    pub fn read_brief_offered(&self) -> bool {
        let here = self.current_project();
        self.project_rows.iter().any(|row| {
            row.understood && row.one_line.is_some() && here.as_deref() != Some(row.name.as_str())
        })
    }

    /// What `read_brief` answers for `project`: the named project's whole
    /// brief as `[read-only · <project>]` and its text, or the message a
    /// refusal carries. The name is matched exactly against the
    /// understood reach rows and this thread's own project, and the path
    /// is built from the matched row's root, so a name that looks like a
    /// path is refused rather than joined onto a root, and a file outside
    /// the reach — or in a workspace this thread does not understand — is
    /// never opened. Reading this thread's own project is symmetry: its
    /// brief is already inline.
    pub fn read_brief(&self, project: &str) -> Result<String, String> {
        if !self.read_brief_offered() {
            return Err(crate::harness_tools::NO_BRIEF_IN_REACH.into());
        }
        if self.current_project().as_deref() == Some(project)
            && let Some(text) = self.layers.project.as_ref().and_then(crate::Project::brief)
        {
            return Ok(crate::harness_tools::brief_result(project, &text));
        }
        let text = self
            .project_rows
            .iter()
            .find(|row| row.understood && row.name == project)
            .and_then(|row| crate::brief::project_brief(&row.root));
        match text {
            Some(text) => Ok(crate::harness_tools::brief_result(project, &text)),
            None => Err(self.no_brief_error(project)),
        }
    }

    /// The names `read_brief` would serve: the understood reach rows that
    /// carry a brief, then this thread's own project when it has one.
    fn brief_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .project_rows
            .iter()
            .filter(|row| row.understood && row.one_line.is_some())
            .map(|row| row.name.clone())
            .collect();
        if let Some(here) = self.current_project()
            && !names.contains(&here)
            && self
                .layers
                .project
                .as_ref()
                .and_then(crate::Project::brief)
                .is_some()
        {
            names.push(here);
        }
        names
    }

    /// The refusal a `read_brief` call gets: why the name does not work,
    /// and the names that would. A name with a separator in it is a path
    /// — unless an understood row carries it, since a related row is
    /// named `<workspace>/<project>`.
    fn no_brief_error(&self, project: &str) -> String {
        let names = self.brief_names();
        let listed = if names.is_empty() {
            "none".to_owned()
        } else {
            names.join(", ")
        };
        let understood = self.current_project().as_deref() == Some(project)
            || self
                .project_rows
                .iter()
                .any(|row| row.understood && row.name == project);
        if !understood && (project.contains('/') || project.contains('\\')) {
            format!(
                "`{project}` is a path, not a project name: read_brief takes the name a project is listed under, and opens it itself. The projects with a brief are: {listed}"
            )
        } else {
            format!(
                "no project this thread understands with a brief is named `{project}`; the projects with a brief are: {listed}"
            )
        }
    }

    /// Reload when the folder changed on disk. Called at the start of a
    /// turn, never mid-turn.
    pub(crate) fn refresh_knowledge(&mut self) -> Result<(), crate::ProjectError> {
        let changed = self
            .layers
            .project
            .as_ref()
            .is_some_and(|p| self.knowledge.changed(&p.knowledge_dir()));
        let workspace_changed = self.layers.workspace.as_ref().is_some_and(|w| {
            match (w.knowledge.as_ref(), w.knowledge_dir()) {
                (Some(loaded), Some(dir)) => loaded.changed(&dir),
                (None, Some(dir)) => crate::knowledge::holds_markdown(&dir),
                _ => false,
            }
        });
        if changed || workspace_changed {
            self.reload_knowledge()?;
        }
        // The rows have not moved since the last turn, but a sibling's
        // memory folder may have: registration is a probe, so run it.
        if let Some(max_hits) = self
            .layers
            .project
            .as_ref()
            .map(|p| p.file.knowledge.max_hits)
        {
            self.install_scopes();
            self.measure_registration(max_hits);
        }
        Ok(())
    }

    /// Re-read the memory files at the start of a turn, so a hand edit
    /// between turns reaches the next prefix (done-when 4). A change
    /// resets the window measure like any other prefix change.
    pub(crate) fn refresh_memory(&mut self) -> Result<(), crate::ProjectError> {
        let before = (
            self.layers.global.memory.clone(),
            self.layers
                .workspace
                .as_ref()
                .map(|w| w.memory.clone())
                .unwrap_or_default(),
            self.layers.project.as_ref().map(|p| p.memory.clone()),
        );
        self.layers.reload_memory()?;
        let after = (
            self.layers.global.memory.clone(),
            self.layers
                .workspace
                .as_ref()
                .map(|w| w.memory.clone())
                .unwrap_or_default(),
            self.layers.project.as_ref().map(|p| p.memory.clone()),
        );
        if before != after {
            self.measured = None;
        }
        Ok(())
    }

    pub fn knowledge(&self) -> &Knowledge {
        &self.knowledge
    }

    /// The resolver the search tool reaches a sibling or the workspace
    /// through; the runtime's own handle on the scopes it installed.
    pub fn scope_sources(&self) -> Arc<crate::knowledge::ScopeSources> {
        self.sources.clone()
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
            person_memory: self.layers.global.memory_block(),
            harness: self.harness_instructions,
            workspace: self.layers.workspace_instructions(),
            workspace_brief: self.layers.workspace.as_ref().and_then(|w| {
                w.brief
                    .as_deref()
                    .map(|text| crate::brief::workspace_block(&w.name, text))
            }),
            project: self.layers.project_instructions(),
            project_brief: self.layers.project.as_ref().and_then(|p| {
                self.project_brief.as_deref().map(|text| {
                    crate::brief::project_block(&p.name, text, self.read_brief_offered())
                })
            }),
            participants: self.participants_line(),
            projects: self.projects.clone(),
            workspace_knowledge: match (
                self.layers.workspace.as_ref(),
                self.workspace_knowledge_mode,
            ) {
                (Some(workspace), Some(mode)) => workspace
                    .knowledge
                    .as_ref()
                    .and_then(|k| k.workspace_prefix(&workspace.name, mode)),
                _ => None,
            },
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

/// One project in reach, as `read_brief` needs it (issue #123): the name
/// the model may ask for, the root the daemon resolved, the workspace it
/// is in and its brief's one line. The daemon builds these; the runtime
/// only looks names up in them, so the tool never follows a path the
/// model wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRow {
    pub name: String,
    pub root: std::path::PathBuf,
    pub workspace: Option<String>,
    /// The brief's first line, `None` when the project has no brief.
    pub one_line: Option<String>,
    /// Whether this thread may read the project at all: its own project,
    /// or one in the thread's own workspace. The daemon decides; a row
    /// outside it is listed but never opened.
    pub understood: bool,
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
    /// The projects in reach for this thread (issue #81, #123), rendered
    /// by the daemon's `ThreadTable`; `None` when the caller has no
    /// listing, which is every caller that is not the daemon.
    pub projects: Option<String>,
    /// The rows the listing was rendered from, so `read_brief` can turn a
    /// name into a root (issue #123); empty when the caller has no
    /// listing.
    pub project_rows: Vec<ProjectRow>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use aigentic_core::{Capabilities, CompletionRequest, ProviderEvent};
    use aigentic_tools::ToolRegistry;

    /// A provider that answers with an empty stream and counts with the
    /// runtime's own estimator, so a test's expected values are the
    /// runtime's arithmetic and never a literal.
    struct Counter;

    impl Provider for Counter {
        fn complete(
            &self,
            _request: &CompletionRequest<'_>,
        ) -> std::pin::Pin<Box<dyn futures_core::Stream<Item = ProviderEvent> + Send + '_>>
        {
            Box::pin(futures_util::stream::empty())
        }
        fn count_tokens(&self, context: &[aigentic_core::Message]) -> u64 {
            aigentic_providers::estimate::estimate_tokens(context)
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                supports_tools: true,
                supports_images: false,
                supports_caching: false,
                supports_structured_output: false,
                max_context_tokens: 1_000,
            }
        }
    }

    /// A runtime over `root`, with a project there when the root has one.
    fn rig(root: &std::path::Path) -> Runtime {
        let log = aigentic_log::ThreadLog::open(root, ulid::Ulid::generate()).unwrap();
        let mut rt = Runtime::new(
            Box::new(Counter),
            ToolRegistry::default(),
            log,
            AgentId("worker".into()),
        );
        if let Ok(Some(project)) = crate::Project::open(root) {
            rt = rt.with_layers(Layers::default().with_project(project));
        }
        rt
    }

    /// A project root with `aigentic.toml` and a brief.
    fn project_dir(brief: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("aigentic.toml"),
            "[project]\nname = \"p\"\n",
        )
        .unwrap();
        if let Some(text) = brief {
            let path = crate::brief::project_brief_path(dir.path());
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
    }

    /// T2 (issue #123): the project's brief is read again at turn start,
    /// and only a change reloads: an unchanged read leaves the window
    /// measure alone, an edit resets it.
    #[test]
    fn refresh_brief_reloads_only_on_a_change() {
        let dir = project_dir(Some("# Foundation\n\nThe site.\n"));
        let mut rt = rig(dir.path());
        rt.refresh_brief();
        assert_eq!(
            rt.project_brief.as_deref(),
            Some("# Foundation\n\nThe site.\n"),
            "the brief was read"
        );

        // A reported call's measure, as `turn.rs` records it: an
        // unchanged read must not touch it.
        rt.measured = Some((1234, 3));
        rt.refresh_brief();
        assert_eq!(
            rt.measured,
            Some((1234, 3)),
            "an unchanged read does not reload"
        );

        let path = crate::brief::project_brief_path(dir.path());
        std::fs::write(&path, "# Foundation\n\nThe site, now Next.js.\n").unwrap();
        rt.refresh_brief();
        assert_eq!(
            rt.project_brief.as_deref(),
            Some("# Foundation\n\nThe site, now Next.js.\n"),
            "the edit reached the runtime"
        );
        assert_eq!(rt.measured, None, "a change resets the window measure");

        // A project with no brief keeps none, and a brief that goes away
        // is a change too.
        std::fs::remove_file(&path).unwrap();
        rt.measured = Some((9, 1));
        rt.refresh_brief();
        assert_eq!(rt.project_brief, None);
        assert_eq!(rt.measured, None, "a removal is a change");
    }

    /// T4 (issue #123): the tool is offered only when some project in
    /// reach other than this thread's own carries a brief, and the
    /// refusal names the reach rows that do. A name that looks like a
    /// path is refused rather than cleaned.
    #[test]
    fn read_brief_is_offered_only_for_a_siblings_brief_and_refuses_a_path() {
        let dir = project_dir(Some("# Foundation\n\nHere.\n"));
        let mut rt = rig(dir.path());
        let here = crate::ProjectRow {
            name: "p".into(),
            root: dir.path().to_path_buf(),
            workspace: None,
            one_line: Some("Here.".into()),
            understood: true,
        };
        let sibling = crate::ProjectRow {
            name: "web".into(),
            root: "/nowhere/web".into(),
            workspace: None,
            one_line: Some("The site.".into()),
            understood: true,
        };
        let briefless = crate::ProjectRow {
            name: "old".into(),
            root: "/nowhere/old".into(),
            workspace: None,
            one_line: None,
            understood: true,
        };

        // The thread's own brief alone is no reason to offer it.
        rt = rt.with_projects(None, vec![here.clone()]);
        assert!(!rt.read_brief_offered());
        let specs = crate::harness_tools::harness_specs_with(false, false, false, false);
        assert!(
            specs
                .iter()
                .all(|s| s.name != crate::harness_tools::READ_BRIEF)
        );

        // A sibling's brief is.
        rt = rt.with_projects(None, vec![here.clone(), sibling, briefless.clone()]);
        assert!(rt.read_brief_offered());
        let names: Vec<String> = crate::harness_tools::harness_specs_with(true, true, true, true)
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert!(
            names.contains(&crate::harness_tools::READ_BRIEF.to_string()),
            "{names:?}"
        );

        // The current project returns its own brief, as a sibling would.
        let own = rt.read_brief("p").unwrap();
        assert!(own.starts_with("[read-only · p]"), "{own}");
        assert!(own.contains("Here."), "{own}");

        // Out of reach, unknown, and a project with no brief: each names
        // the rows that do have one — the sibling and this project — and
        // never the brief-less row.
        for name in ["other", "old"] {
            let answer = rt.read_brief(name).unwrap_err();
            // The list names the rows with a brief, `p` and `web`, and
            // never the brief-less `old`.
            let listed = answer.rsplit(": ").next().unwrap();
            assert!(listed.contains("p") && listed.contains("web"), "{answer}");
            assert!(!listed.contains("old"), "{answer}");
        }

        // A path, a traversal and a name with a separator: refused, not
        // cleaned, and called a path.
        for name in ["../x", "/etc/passwd", "web/../p"] {
            let answer = rt.read_brief(name).unwrap_err();
            assert!(
                answer.contains("is a path, not a project name"),
                "{name}: {answer}"
            );
            assert!(answer.contains("web"), "{name}: {answer}");
        }
        // An empty name is no path; it is simply not a project.
        let answer = rt.read_brief("").unwrap_err();
        assert!(answer.contains("is named ``"), "{answer}");
    }

    /// T5 (issue #129): a related row is addressed by the name the block
    /// lists it under, `<workspace>/<project>`. That name wins over the
    /// path clause, while a separator name no row carries stays a path.
    #[test]
    fn a_name_with_a_separator_is_briefable_when_a_row_carries_it() {
        let here = project_dir(Some("# Foundation\n\nHere.\n"));
        let related = project_dir(Some("# Elsewhere\n\nThe related brief.\n"));
        let here_row = crate::ProjectRow {
            name: "p".into(),
            root: here.path().to_path_buf(),
            workspace: None,
            one_line: Some("Here.".into()),
            understood: true,
        };
        let related_row = crate::ProjectRow {
            name: "w/q".into(),
            root: related.path().to_path_buf(),
            workspace: Some("w".into()),
            one_line: Some("Elsewhere.".into()),
            understood: true,
        };
        let rt = rig(here.path()).with_projects(None, vec![here_row.clone(), related_row.clone()]);
        assert!(rt.read_brief_offered(), "the related row carries a brief");
        let served = rt.read_brief("w/q").unwrap();
        assert_eq!(
            served,
            crate::harness_tools::brief_result("w/q", "# Elsewhere\n\nThe related brief.\n"),
            "served as a read-only hit under its listed name"
        );

        // The row is understood but has no brief: the miss sentence, not
        // the path one. A second, briefed row keeps the tool offered.
        let briefless = crate::ProjectRow {
            name: "w/q".into(),
            root: "/nowhere/q".into(),
            workspace: Some("w".into()),
            one_line: None,
            understood: true,
        };
        let web = crate::ProjectRow {
            name: "web".into(),
            root: related.path().to_path_buf(),
            workspace: Some("w".into()),
            one_line: Some("Elsewhere.".into()),
            understood: true,
        };
        let rt = rig(here.path()).with_projects(None, vec![here_row, web, briefless]);
        assert!(rt.read_brief_offered(), "the briefed row offers the tool");
        let answer = rt.read_brief("w/q").unwrap_err();
        assert_eq!(
            answer,
            "no project this thread understands with a brief is named `w/q`; the projects with a brief are: p, web",
            "the miss sentence, and the list names the briefed rows"
        );
        assert!(!answer.contains("is a path"), "{answer}");

        // No row carries it: still the path clause.
        let answer = rt.read_brief("v/s").unwrap_err();
        assert!(answer.contains("is a path, not a project name"), "{answer}");
    }
}
