use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Per-turn limits. A turn that hits one ends with a `turn_ended` event
/// carrying the reason, never with a silent stop.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Budget {
    pub max_iterations: u32,
    pub max_tokens: u64,
    pub max_wall_time: Duration,
    /// What a cache read counts against `max_tokens`, as a fraction of
    /// an uncached input token: providers price a re-read prefix far
    /// below fresh input, so counting both the same spends a build
    /// turn's budget on tokens that were nearly free. A quarter is the
    /// usual price (TensorX: $0.44/M cache reads against $1.75/M
    /// uncached input); a profile sets its own in
    /// `[profiles.<name>.budget]`.
    #[serde(default = "quarter")]
    pub cache_read_price_ratio: f64,
}

fn quarter() -> f64 {
    0.25
}

impl Budget {
    /// What one call spends of `max_tokens`: uncached input, cache
    /// writes and output in full, cache reads at
    /// `cache_read_price_ratio`.
    pub fn spent_of(&self, usage: &crate::Usage) -> u64 {
        usage.input_tokens
            + usage.cache_write_tokens
            + usage.output_tokens
            + (usage.cache_read_tokens as f64 * self.cache_read_price_ratio) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::Budget;
    use crate::Usage;
    use std::time::Duration;

    #[test]
    fn cache_reads_spend_at_the_budgets_own_ratio() {
        let usage = Usage {
            input_tokens: 200,
            output_tokens: 300,
            cache_read_tokens: 140_000,
            cache_write_tokens: 500,
            ..Default::default()
        };
        let budget = |ratio| Budget {
            max_iterations: 1,
            max_tokens: 1,
            max_wall_time: Duration::from_secs(1),
            cache_read_price_ratio: ratio,
        };
        assert_eq!(budget(0.25).spent_of(&usage), 200 + 300 + 500 + 35_000);
        assert_eq!(budget(0.5).spent_of(&usage), 200 + 300 + 500 + 70_000);
    }
}
