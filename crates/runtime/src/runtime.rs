use std::time::Duration;

use aigentic_core::{AgentId, Budget, Event, Provider, Tool, ToolCall};
use aigentic_log::ThreadLog;

/// Conservative per-turn defaults; projects raise them in phase 4.
pub const DEFAULT_BUDGET: Budget = Budget {
    max_iterations: 20,
    max_tokens: 200_000,
    max_wall_time: Duration::from_secs(600),
};

/// One thread's runtime: the provider, the tool registry and the single
/// writer for that thread's log.
pub struct Runtime {
    pub(crate) provider: Box<dyn Provider>,
    pub(crate) tools: Vec<Box<dyn Tool>>,
    pub(crate) log: ThreadLog,
    pub(crate) agent: AgentId,
    pub(crate) budget: Budget,
    pub(crate) instructions: Option<String>,
}

impl Runtime {
    pub fn new(
        provider: Box<dyn Provider>,
        tools: Vec<Box<dyn Tool>>,
        log: ThreadLog,
        agent: AgentId,
    ) -> Self {
        Self {
            provider,
            tools,
            log,
            agent,
            budget: DEFAULT_BUDGET,
            instructions: None,
        }
    }

    pub fn with_budget(mut self, budget: Budget) -> Self {
        self.budget = budget;
        self
    }

    /// Repository instructions for the stable prefix; see
    /// [`load_instructions`](crate::load_instructions).
    pub fn with_instructions(mut self, instructions: Option<String>) -> Self {
        self.instructions = instructions;
        self
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
