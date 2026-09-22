//! The status line (plan section 6): what the daemon reports, never
//! what the client counts. Context fill is the runtime's measured
//! window fill against the provider's window; elapsed is the running
//! turn's clock; the queue is the daemon's.

use std::time::Duration;

use aigentic_api::ThreadState;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// The thread's project.
    pub project: String,
    pub mode: String,
    /// (tokens in the window, the window), from `Notice::Usage`.
    pub usage: Option<(u64, u64)>,
    /// How long the running turn has run; `None` when idle.
    pub elapsed: Option<Duration>,
    pub queued: u32,
    /// What the thread waits on, when it does.
    pub waiting: Option<&'static str>,
}

impl Status {
    /// Context used, as a whole percentage, or `None` before the first
    /// fill was reported.
    pub fn context_percent(&self) -> Option<u64> {
        let (used, window) = self.usage?;
        if window == 0 {
            return None;
        }
        Some((used.saturating_mul(100) / window).min(100))
    }

    /// `manual · vendela · 31% context · 12s · queued 1`
    pub fn line(&self) -> String {
        let mut parts = vec![self.mode.clone(), self.project.clone()];
        match self.context_percent() {
            Some(p) => parts.push(format!("{p}% context")),
            None => parts.push("context ?".into()),
        }
        if let Some(elapsed) = self.elapsed {
            parts.push(elapsed_short(elapsed));
        }
        if self.queued > 0 {
            parts.push(format!("queued {}", self.queued));
        }
        if let Some(w) = self.waiting {
            parts.push(w.to_owned());
        }
        parts.join(" · ")
    }

    /// The parts that come from the thread's state.
    pub fn apply_state(&mut self, state: &ThreadState) {
        match state {
            ThreadState::Idle => {
                self.queued = 0;
                self.waiting = None;
            }
            ThreadState::Running { queued, .. } => {
                self.queued = *queued;
                self.waiting = None;
            }
            ThreadState::AwaitingApproval { .. } => self.waiting = Some("awaiting approval"),
            ThreadState::AwaitingHuman { .. } => self.waiting = Some("awaiting an answer"),
        }
    }
}

/// `12s`, `1m 05s`, `1h 02m`.
pub fn elapsed_short(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_line_reads_left_to_right() {
        let mut s = Status {
            project: "vendela".into(),
            mode: "manual".into(),
            usage: Some((31_000, 100_000)),
            elapsed: Some(Duration::from_secs(12)),
            queued: 1,
            waiting: None,
        };
        assert_eq!(s.line(), "manual · vendela · 31% context · 12s · queued 1");
        s.elapsed = None;
        s.queued = 0;
        s.usage = None;
        assert_eq!(s.line(), "manual · vendela · context ?");
        s.usage = Some((250_000, 100_000));
        assert_eq!(s.context_percent(), Some(100));
    }

    #[test]
    fn elapsed_formats() {
        assert_eq!(elapsed_short(Duration::from_secs(5)), "5s");
        assert_eq!(elapsed_short(Duration::from_secs(65)), "1m 05s");
        assert_eq!(elapsed_short(Duration::from_secs(3725)), "1h 02m");
    }

    #[test]
    fn state_sets_queue_and_waiting() {
        let mut s = Status::default();
        s.apply_state(&ThreadState::Running {
            by: aigentic_runtime::aigentic_core::Author::System,
            queued: 2,
        });
        assert_eq!(s.queued, 2);
        s.apply_state(&ThreadState::AwaitingHuman {
            call_id: "c".into(),
            question: "q".into(),
        });
        assert_eq!(s.waiting, Some("awaiting an answer"));
        s.apply_state(&ThreadState::Idle);
        assert_eq!(s.queued, 0);
        assert_eq!(s.waiting, None);
    }
}
