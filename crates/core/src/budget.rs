use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Per-turn limits. A turn that hits one ends with a `turn_ended` event
/// carrying the reason, never with a silent stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    pub max_iterations: u32,
    pub max_tokens: u64,
    pub max_wall_time: Duration,
}
