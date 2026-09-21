use serde::{Deserialize, Serialize};

use crate::BoxFuture;

/// How dangerous a tool is; drives permission prompts in the policy crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskClass {
    Read,
    Write,
    Exec,
    Network,
    Safe,
}

/// What a tool returns to the loop; recorded as a `tool_result` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

/// Errors a tool can surface.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ToolError {
    #[error("invalid arguments: {0}")]
    InvalidArgs(String),
    #[error("execution failed: {0}")]
    Execution(String),
    #[error("timed out")]
    Timeout,
    #[error("cancelled")]
    Cancelled,
}

/// A callable tool. Object-safe so the registry can hold `Box<dyn Tool>`.
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> schemars::schema::RootSchema;
    fn risk_class(&self) -> RiskClass;
    fn call(&self, args: serde_json::Value) -> BoxFuture<'_, Result<ToolOutput, ToolError>>;
}
